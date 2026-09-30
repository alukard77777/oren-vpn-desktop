//! The service updates the Windows install itself: it downloads the release zip, checks the
//! GitHub digest and the in-zip `SHA256SUMS.txt`, and hands over to `multicore-updater` running
//! as SYSTEM, which swaps `current`, restarts the service and relaunches the desktop in the
//! user's session. Linux packages are updated by the package manager instead.
//!
//! The session and the user's update directory handed to the updater come from the
//! authenticated client (its token's session in `user_key`, its known-folder `data_root`),
//! never from the request.

use multicore_core::elevation_protocol::BrokerErrorCode;
use multicore_release::Version;

use super::core_loop::ClientIdentity;

/// Only a strictly newer release is accepted.
pub(crate) fn plan_update(current: &str, requested: &str) -> Result<(), BrokerErrorCode> {
    let current = Version::parse(current).map_err(|_| BrokerErrorCode::NotReady)?;
    let requested = Version::parse(requested).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    if requested > current {
        Ok(())
    } else {
        Err(BrokerErrorCode::InvalidRequest)
    }
}

/// Windows user keys are `"<token session>:<sid>"`; a Unix key (`"uid:<uid>"`) has none.
pub(crate) fn session_of(client: &ClientIdentity) -> Option<u32> {
    client.user_key.split_once(':')?.0.parse().ok()
}

#[cfg(windows)]
pub(crate) use production::ProductionUpdater;

#[cfg(windows)]
mod production {
    use std::{
        fs::{self, File},
        io::Write,
        os::windows::{
            io::{AsRawHandle, OwnedHandle},
            process::CommandExt,
        },
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::Arc,
        thread,
        time::Duration,
    };

    use multicore_core::elevation_protocol::BrokerErrorCode;
    use multicore_release::{
        GitHubRelease, Repository, extract_verified_bundle, verify_archive_file,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, DETACHED_PROCESS, SetEvent};

    use super::{ClientIdentity, plan_update, session_of};
    use crate::service::core_loop::ServiceUpdater;

    const RELEASE_BODY_LIMIT: usize = 512 * 1024;
    const USER_AGENT: &str = concat!("multicore-service/", env!("MULTICORE_APP_VERSION"));
    /// Must match the desktop's `updater::update_directory()` (`<data root>\updates`).
    const USER_UPDATE_DIRECTORY: &str = "updates";
    /// Time for the connection to answer `UpdateStarted` before the service stops itself.
    const HANDOVER_DELAY: Duration = Duration::from_secs(2);

    /// A downloaded and verified release, waiting for the cores to stop.
    struct Staged {
        version: String,
        archive: PathBuf,
        size: u64,
        sha256: String,
    }

    pub(crate) struct ProductionUpdater {
        service_directory: PathBuf,
        package_root: PathBuf,
        stop: Arc<OwnedHandle>,
        runtime: Option<tokio::runtime::Runtime>,
        staged: Option<Staged>,
        launched: bool,
    }

    impl ProductionUpdater {
        /// `package_root` is the install's `current` directory (the updater's `--target`).
        pub(crate) fn new(
            service_directory: PathBuf,
            package_root: PathBuf,
            stop: Arc<OwnedHandle>,
        ) -> Self {
            Self {
                service_directory,
                package_root,
                stop,
                runtime: tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok(),
                staged: None,
                launched: false,
            }
        }

        fn update_directory(&self) -> PathBuf {
            self.service_directory.join("update")
        }

        fn fetch(&self, repository: &Repository) -> Result<GitHubRelease, BrokerErrorCode> {
            let runtime = self.runtime.as_ref().ok_or(BrokerErrorCode::NotReady)?;
            runtime.block_on(async {
                let client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(8))
                    .timeout(Duration::from_secs(30))
                    .redirect(reqwest::redirect::Policy::limited(2))
                    .build()
                    .map_err(|_| BrokerErrorCode::NotReady)?;
                let mut response = client
                    .get(repository.latest_release_api_url())
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header("User-Agent", USER_AGENT)
                    .send()
                    .await
                    .map_err(|_| BrokerErrorCode::NotReady)?;
                if !response.status().is_success() {
                    return Err(BrokerErrorCode::NotReady);
                }
                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| BrokerErrorCode::NotReady)?
                {
                    body.extend_from_slice(&chunk);
                    if body.len() > RELEASE_BODY_LIMIT {
                        return Err(BrokerErrorCode::InvalidRequest);
                    }
                }
                GitHubRelease::parse_for_repository(repository, &body)
                    .map_err(|_| BrokerErrorCode::InvalidRequest)
            })
        }

        fn download(
            &self,
            release: &GitHubRelease,
            destination: &Path,
        ) -> Result<(), BrokerErrorCode> {
            let runtime = self.runtime.as_ref().ok_or(BrokerErrorCode::NotReady)?;
            runtime.block_on(async {
                let client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(8))
                    .timeout(Duration::from_secs(600))
                    .redirect(reqwest::redirect::Policy::custom(|attempt| {
                        let allowed = attempt.url().scheme() == "https"
                            && matches!(
                                attempt.url().host_str(),
                                Some("github.com")
                                    | Some("release-assets.githubusercontent.com")
                                    | Some("objects.githubusercontent.com")
                            );
                        if allowed && attempt.previous().len() < 5 {
                            attempt.follow()
                        } else {
                            attempt.stop()
                        }
                    }))
                    .build()
                    .map_err(|_| BrokerErrorCode::NotReady)?;
                let mut response = client
                    .get(release.asset().download_url())
                    .header("User-Agent", USER_AGENT)
                    .send()
                    .await
                    .map_err(|_| BrokerErrorCode::NotReady)?;
                if !response.status().is_success() {
                    return Err(BrokerErrorCode::NotReady);
                }
                let mut output =
                    File::create(destination).map_err(|_| BrokerErrorCode::NotReady)?;
                let mut written = 0_u64;
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| BrokerErrorCode::NotReady)?
                {
                    written += chunk.len() as u64;
                    if written > release.asset().size() {
                        return Err(BrokerErrorCode::InvalidRequest);
                    }
                    output
                        .write_all(&chunk)
                        .map_err(|_| BrokerErrorCode::NotReady)?;
                }
                output.flush().map_err(|_| BrokerErrorCode::NotReady)?;
                Ok(())
            })
        }

        fn stage(&mut self, version: &str) -> Result<Staged, BrokerErrorCode> {
            let repository = option_env!("MULTICORE_UPDATE_REPOSITORY")
                .and_then(|value| Repository::parse(value).ok())
                .ok_or(BrokerErrorCode::Unsupported)?;
            multicore_core::ensure_crypto_provider_installed();
            let release = self.fetch(&repository)?;
            if release.version().to_string() != version {
                return Err(BrokerErrorCode::InvalidRequest);
            }
            let update_dir = self.update_directory();
            let _ = fs::remove_dir_all(&update_dir);
            fs::create_dir_all(&update_dir).map_err(|_| BrokerErrorCode::NotReady)?;
            let archive = update_dir.join(format!("multicore-{version}.zip"));
            self.download(&release, &archive)?;
            verify_archive_file(
                &archive,
                release.asset().size(),
                release.asset().sha256_hex(),
            )
            .map_err(|_| BrokerErrorCode::InvalidRequest)?;
            // The in-zip SHA256SUMS.txt check: extract once into a scratch directory.
            let check = update_dir.join("verify");
            let verified = extract_verified_bundle(&archive, &check);
            let _ = fs::remove_dir_all(&check);
            verified.map_err(|_| BrokerErrorCode::InvalidRequest)?;
            Ok(Staged {
                version: version.to_owned(),
                archive,
                size: release.asset().size(),
                sha256: release.asset().sha256_hex().to_owned(),
            })
        }
    }

    impl ServiceUpdater for ProductionUpdater {
        fn prepare(
            &mut self,
            client: &ClientIdentity,
            version: &str,
        ) -> Result<(), BrokerErrorCode> {
            if self.launched {
                return Err(BrokerErrorCode::NotReady);
            }
            plan_update(env!("MULTICORE_APP_VERSION"), version)?;
            session_of(client)
                .filter(|session| *session != 0)
                .ok_or(BrokerErrorCode::InvalidRequest)?;
            self.staged = None;
            self.staged = Some(self.stage(version)?);
            Ok(())
        }

        fn begin(&mut self, client: &ClientIdentity, version: &str) -> Result<(), BrokerErrorCode> {
            if self.launched {
                return Err(BrokerErrorCode::NotReady);
            }
            let staged = self
                .staged
                .take()
                .filter(|staged| staged.version == version)
                .ok_or(BrokerErrorCode::NotReady)?;
            let session = session_of(client)
                .filter(|session| *session != 0)
                .ok_or(BrokerErrorCode::InvalidRequest)?;
            let update_dir = self.update_directory();
            let helper = update_dir.join("multicore-updater.exe");
            fs::copy(
                self.package_root
                    .join("runtime")
                    .join("multicore-updater.exe"),
                &helper,
            )
            .map_err(|_| BrokerErrorCode::NotReady)?;
            Command::new(&helper)
                .arg("--wait-pid")
                .arg(std::process::id().to_string())
                .arg("--archive")
                .arg(&staged.archive)
                .arg("--target")
                .arg(&self.package_root)
                .arg("--size")
                .arg(staged.size.to_string())
                .arg("--sha256")
                .arg(&staged.sha256)
                .arg("--version")
                .arg(&staged.version)
                .arg("--service-session")
                .arg(session.to_string())
                .arg("--user-update-dir")
                .arg(client.data_root.join(USER_UPDATE_DIRECTORY))
                .current_dir(&update_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
                .spawn()
                .map_err(|_| BrokerErrorCode::NotReady)?;
            self.launched = true;
            // The updater waits for this process to exit before touching `current`, and
            // starts the service again afterwards (or after a rollback).
            let stop = self.stop.clone();
            thread::spawn(move || {
                thread::sleep(HANDOVER_DELAY);
                unsafe { SetEvent(stop.as_raw_handle()) };
            });
            Ok(())
        }
    }
}

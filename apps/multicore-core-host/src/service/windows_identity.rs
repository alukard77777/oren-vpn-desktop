//! Who is on the other end of the service pipe: the installed daemon image (next to this
//! service in the admin-owned install directory), running under an interactive logon (console
//! or RDP), and whose `%LOCALAPPDATA%` holds the staged generation. Every fact comes from the
//! client's process and token, never from anything the client sends.

use std::{
    ffi::OsString,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf},
    ptr,
};

use windows_sys::Win32::{
    Foundation::{HANDLE, LocalFree},
    Security::{
        Authorization::ConvertSidToStringSidW, CreateWellKnownSid, EqualSid, GetTokenInformation,
        RevertToSelf, SECURITY_MAX_SID_SIZE, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_IMPERSONATE,
        TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenElevationType,
        TokenElevationTypeDefault, TokenGroups, TokenSessionId, TokenUser, WELL_KNOWN_SID_TYPE,
        WinBuiltinAdministratorsSid, WinInteractiveSid,
    },
    System::{
        Com::CoTaskMemFree,
        Pipes::{GetNamedPipeClientProcessId, ImpersonateNamedPipeClient},
        Threading::{
            GetCurrentThread, OpenProcess, OpenProcessToken, OpenThreadToken, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
    },
    UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

/// `SE_GROUP_ENABLED` / `SE_GROUP_USE_FOR_DENY_ONLY` (winnt.h).
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x0000_0010;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum IdentityError {
    WrongImage,
    WrongSession,
    Unavailable,
}

#[derive(Debug)]
pub(crate) struct ClientFacts {
    #[allow(dead_code, reason = "kept for diagnostics and pinned by the tests")]
    pub(crate) process_id: u32,
    pub(crate) image: PathBuf,
    /// `TokenSessionId` of the client's primary token.
    pub(crate) session_id: u32,
    pub(crate) user_sid: String,
    /// The token carries the enabled INTERACTIVE group (`S-1-5-4`): a console or RDP logon.
    pub(crate) interactive: bool,
    /// The user is an administrator: a UAC-split (filtered or elevated) token, or an enabled
    /// Administrators group (UAC off).
    pub(crate) administrator: bool,
}

/// `<install>\current\runtime\multicore-core-host.exe` → `<install>\current\runtime\multicore-daemon.exe`.
///
/// The service only trusts a daemon from the install directory it runs from itself, and only
/// when it runs from `current\runtime` (an old backup or a development build accepts no one).
pub(crate) fn expected_daemon_image(host_executable: &Path) -> Option<PathBuf> {
    if !host_executable.is_absolute() {
        return None;
    }
    let runtime = host_executable.parent()?;
    let current = runtime.parent()?;
    let install = current.parent()?;
    let named = |path: &Path, name: &str| {
        path.file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(name))
    };
    if !named(runtime, "runtime") || !named(current, "current") {
        return None;
    }
    Some(
        install
            .join("current")
            .join("runtime")
            .join("multicore-daemon.exe"),
    )
}

/// Case-insensitive Win32 form without the `\\?\` prefix, for comparisons only.
pub(crate) fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_lowercase()
}

fn has_dot_components(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
}

pub(crate) fn verify_client(
    facts: &ClientFacts,
    expected_daemon: &Path,
) -> Result<(), IdentityError> {
    if has_dot_components(&facts.image) || normalized(&facts.image) != normalized(expected_daemon) {
        return Err(IdentityError::WrongImage);
    }
    // Session 0 hosts services only; network and service logons lack INTERACTIVE.
    if !facts.interactive || facts.session_id == 0 {
        return Err(IdentityError::WrongSession);
    }
    Ok(())
}

/// Facts about the pipe's client process and its primary token. The token is returned so the
/// caller can resolve the user's known folders with it.
pub(crate) fn client_facts(pipe: HANDLE) -> Result<(ClientFacts, OwnedHandle), IdentityError> {
    let mut process_id = 0;
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut process_id) } == 0 {
        return Err(IdentityError::Unavailable);
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(IdentityError::Unavailable);
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &mut length,
        )
    } == 0
    {
        return Err(IdentityError::Unavailable);
    }
    let image = PathBuf::from(OsString::from_wide(&buffer[..length as usize]));
    let mut token = ptr::null_mut();
    if unsafe {
        OpenProcessToken(
            process.as_raw_handle(),
            TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_DUPLICATE,
            &mut token,
        )
    } == 0
    {
        return Err(IdentityError::Unavailable);
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let session_id = token_session(token.as_raw_handle())?;
    let user_sid = token_user_sid(token.as_raw_handle())?;
    let interactive = token_is_interactive(token.as_raw_handle())?;
    let administrator = token_is_administrator(token.as_raw_handle())?;
    // The PID may have been recycled since the client connected: the process we opened must
    // run as the same user, in the same session, as the token the client connected with.
    let (connected_sid, connected_session) = connection_token_facts(pipe)?;
    if !connected_sid.eq_ignore_ascii_case(&user_sid) || connected_session != session_id {
        return Err(IdentityError::Unavailable);
    }
    Ok((
        ClientFacts {
            process_id,
            image,
            session_id,
            user_sid,
            interactive,
            administrator,
        },
        token,
    ))
}

/// User SID and session of the token the client connected with (identification level is
/// enough to query it). Requires that a message was already read from the pipe.
fn connection_token_facts(pipe: HANDLE) -> Result<(String, u32), IdentityError> {
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(IdentityError::Unavailable);
    }
    let mut token = ptr::null_mut();
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
    if unsafe { RevertToSelf() } == 0 {
        // Never keep running under the client's identity.
        std::process::abort();
    }
    if opened == 0 {
        return Err(IdentityError::Unavailable);
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    Ok((
        token_user_sid(token.as_raw_handle())?,
        token_session(token.as_raw_handle())?,
    ))
}

/// `GetTokenInformation` into a pointer-aligned buffer.
fn token_information(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<Vec<usize>, IdentityError> {
    let mut needed = 0;
    unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(IdentityError::Unavailable);
    }
    let mut buffer = vec![0_usize; (needed as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * size_of::<usize>()) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(IdentityError::Unavailable);
    }
    Ok(buffer)
}

fn token_session(token: HANDLE) -> Result<u32, IdentityError> {
    let mut session = 0_u32;
    let mut needed = 0;
    if unsafe {
        GetTokenInformation(
            token,
            TokenSessionId,
            (&mut session as *mut u32).cast(),
            size_of::<u32>() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(IdentityError::Unavailable);
    }
    Ok(session)
}

fn token_user_sid(token: HANDLE) -> Result<String, IdentityError> {
    let buffer = token_information(token, TokenUser)?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(IdentityError::Unavailable);
    }
    let mut length = 0;
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}

/// The token's groups contain INTERACTIVE (`S-1-5-4`), enabled and not deny-only.
fn token_is_interactive(token: HANDLE) -> Result<bool, IdentityError> {
    token_has_enabled_group(token, WinInteractiveSid)
}

/// Only users UAC splits a token for (administrators) have a non-default elevation type; with
/// UAC off an administrator's token simply has the Administrators group enabled.
fn token_is_administrator(token: HANDLE) -> Result<bool, IdentityError> {
    let mut elevation_type = 0_i32;
    let mut needed = 0;
    if unsafe {
        GetTokenInformation(
            token,
            TokenElevationType,
            (&mut elevation_type as *mut i32).cast(),
            size_of::<i32>() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(IdentityError::Unavailable);
    }
    if elevation_type != TokenElevationTypeDefault {
        return Ok(true);
    }
    token_has_enabled_group(token, WinBuiltinAdministratorsSid)
}

fn token_has_enabled_group(
    token: HANDLE,
    group: WELL_KNOWN_SID_TYPE,
) -> Result<bool, IdentityError> {
    let mut interactive = [0_usize; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<usize>())];
    let mut size = SECURITY_MAX_SID_SIZE;
    if unsafe {
        CreateWellKnownSid(
            group,
            ptr::null_mut(),
            interactive.as_mut_ptr().cast(),
            &mut size,
        )
    } == 0
    {
        return Err(IdentityError::Unavailable);
    }
    let buffer = token_information(token, TokenGroups)?;
    let groups = unsafe { &*buffer.as_ptr().cast::<TOKEN_GROUPS>() };
    let entries =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    Ok(entries.iter().any(|group| {
        group.Attributes & SE_GROUP_ENABLED != 0
            && group.Attributes & SE_GROUP_USE_FOR_DENY_ONLY == 0
            && unsafe { EqualSid(group.Sid, interactive.as_mut_ptr().cast()) } != 0
    }))
}

/// `%LOCALAPPDATA%\MultiCore` of the token's user, resolved by the shell for that user.
pub(crate) fn user_data_root(token: HANDLE) -> Result<PathBuf, IdentityError> {
    let mut raw = ptr::null_mut();
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DEFAULT as u32,
            token,
            &mut raw,
        )
    };
    if result < 0 || raw.is_null() {
        if !raw.is_null() {
            unsafe { CoTaskMemFree(raw.cast()) };
        }
        return Err(IdentityError::Unavailable);
    }
    let mut length = 0;
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    let root = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    unsafe { CoTaskMemFree(raw.cast()) };
    if !root.is_absolute() {
        return Err(IdentityError::Unavailable);
    }
    Ok(root.join("MultiCore"))
}

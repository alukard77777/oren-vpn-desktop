include!("../../build-support/windows_version_info.rs");

const APP_ICON_SVG: &[u8] = include_bytes!("assets/app-icon.svg");
const TRAY_ICON_SIZES: [u32; 2] = [32, 64];
/// The tray's connection states: a badge in the logo's empty lower-left corner, told apart by
/// its shape (dot, ring, open arc, diamond) as well as its colour, so a monochrome or
/// colour-blind look still reads it. «Disconnected» is the plain logo.
const TRAY_STATE_BADGES: [(&str, &str); 4] = [
    // Connected through the system proxy: a filled dot.
    (
        "proxy",
        r##"<circle cx="13" cy="51" r="11" fill="#3CCB7F"/>"##,
    ),
    // Connected through TUN (all traffic): a ring.
    (
        "tun",
        r##"<circle cx="13" cy="51" r="8.5" stroke="#3CCB7F" stroke-width="5"/>"##,
    ),
    // Connecting or disconnecting: an open arc, like a spinner.
    (
        "connecting",
        r##"<path d="M13 42.5A8.5 8.5 0 1 0 20.36 55.25" stroke="#E7AA45" stroke-width="5" stroke-linecap="round"/>"##,
    ),
    // Error: a diamond.
    (
        "error",
        r##"<path d="M13 39.5L24.5 51L13 62.5L1.5 51Z" fill="#F06A6A"/>"##,
    ),
];
/// Sizes Windows draws a tray icon at (100–200 % scaling), for the offscreen previews.
const TRAY_PREVIEW_SIZES: [u32; 4] = [16, 20, 24, 32];

fn main() {
    println!("cargo:rerun-if-env-changed=MULTICORE_UPDATE_REPOSITORY");
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let output_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    let icon_tree = resvg::usvg::Tree::from_data(APP_ICON_SVG, &resvg::usvg::Options::default())
        .expect("failed to parse the canonical MultiCore SVG icon");
    for size in TRAY_ICON_SIZES {
        let tray_icon = rasterize_app_icon(&icon_tree, size);
        std::fs::write(output_dir.join(format!("app-icon-{size}.rgba")), tray_icon)
            .expect("failed to write the generated MultiCore tray icon");
    }
    // `MULTICORE_TRAY_PREVIEW_DIR`: also writes every state at the Windows tray sizes as raw
    // RGBA (`<state>-<size>.rgba`), to review the badges offscreen.
    println!("cargo:rerun-if-env-changed=MULTICORE_TRAY_PREVIEW_DIR");
    let preview_dir = std::env::var_os("MULTICORE_TRAY_PREVIEW_DIR").map(std::path::PathBuf::from);
    if let Some(preview_dir) = &preview_dir {
        std::fs::create_dir_all(preview_dir).expect("failed to create the tray preview directory");
        for size in TRAY_PREVIEW_SIZES {
            std::fs::write(
                preview_dir.join(format!("disconnected-{size}.rgba")),
                rasterize_app_icon(&icon_tree, size),
            )
            .expect("failed to write a tray preview");
        }
    }
    for (state, badge) in TRAY_STATE_BADGES {
        let tree = resvg::usvg::Tree::from_data(
            tray_state_svg(badge).as_bytes(),
            &resvg::usvg::Options::default(),
        )
        .expect("failed to parse a MultiCore tray state icon");
        for size in TRAY_ICON_SIZES {
            std::fs::write(
                output_dir.join(format!("app-icon-{state}-{size}.rgba")),
                rasterize_app_icon(&tree, size),
            )
            .expect("failed to write a MultiCore tray state icon");
        }
        if let Some(preview_dir) = &preview_dir {
            for size in TRAY_PREVIEW_SIZES {
                std::fs::write(
                    preview_dir.join(format!("{state}-{size}.rgba")),
                    rasterize_app_icon(&tree, size),
                )
                .expect("failed to write a tray preview");
            }
        }
    }

    let components_path = manifest_dir
        .join("ui/components.slint")
        .display()
        .to_string()
        .replace('\\', "/");
    let check_source = format!(
        r#"import {{ PrimaryAction, QuietChip, RouteNodeRow }} from "{components_path}";
export component ComponentCheck inherits Window {{
    preferred-width: 480px;
    preferred-height: 720px;
    PrimaryAction {{ label: "Подключить"; state-label: "Подключить профиль"; }}
    QuietChip {{ label: "Автовыбор"; selected: true; }}
    RouteNodeRow {{ label: "Новосибирск"; trailing-text: "42 мс"; pending: true; }}
}}
"#,
    );
    let check_input = output_dir.join("multicore_components_check.slint");
    std::fs::write(&check_input, check_source)
        .expect("failed to create the MultiCore component validation source");

    slint_build::compile_with_output_path(
        check_input,
        output_dir.join("multicore_components_check.rs"),
        slint_build::CompilerConfiguration::new()
            // The app is dark-only; the plain "fluent" style follows the OS
            // color scheme and drew white LineEdits/scrollbars on a light
            // Windows theme.
            .with_style("fluent-dark".into())
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles),
    )
    .expect("failed to validate the MultiCore component library");
    println!("cargo:rerun-if-changed=ui/components.slint");
    println!("cargo:rerun-if-changed=ui/theme.slint");
    println!("cargo:rerun-if-changed=assets/fonts/Twemoji.Mozilla.ttf");
    println!("cargo:rerun-if-changed=assets/power.svg");
    println!("cargo:rerun-if-changed=assets/link.svg");
    println!("cargo:rerun-if-changed=assets/edit.svg");
    println!("cargo:rerun-if-changed=assets/diagnostics.svg");
    println!("cargo:rerun-if-changed=assets/app-icon.svg");
    println!("cargo:rerun-if-changed=assets/close.svg");
    println!("cargo:rerun-if-changed=assets/refresh.svg");
    println!("cargo:rerun-if-changed=assets/trash.svg");
    println!("cargo:rerun-if-changed=../../build-support/windows_version_info.rs");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app-debug.manifest");

    emit_runtime_versions(&manifest_dir);

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        compile_windows_resources(&manifest_dir, &output_dir, &icon_tree);
    }

    // English is bundled from the same catalog the Rust strings use (`multicore-i18n`). Russian
    // is the source language: `@tr("…")` msgids are the exact Russian text, with no context.
    println!("cargo:rerun-if-changed=translations/en/LC_MESSAGES/multicore-desktop.po");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            // The app is dark-only; the plain "fluent" style follows the OS
            // color scheme and drew white LineEdits/scrollbars on a light
            // Windows theme.
            .with_style("fluent-dark".into())
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
            .with_bundled_translations(manifest_dir.join("translations"))
            .with_default_translation_context(slint_build::DefaultTranslationContext::None),
    )
    .expect("failed to compile the Slint UI");
}

fn compile_windows_resources(
    manifest_dir: &std::path::Path,
    output_dir: &std::path::Path,
    icon_tree: &resvg::usvg::Tree,
) {
    let icon_path = output_dir.join("multicore.ico");
    let icon_file = std::fs::File::create(&icon_path).expect("failed to create MultiCore icon");
    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 20, 24, 32, 48, 64, 128, 256] {
        let image = ico::IconImage::from_rgba_data(size, size, rasterize_app_icon(icon_tree, size));
        icon.add_entry(ico::IconDirEntry::encode(&image).expect("failed to encode icon frame"));
    }
    icon.write(icon_file)
        .expect("failed to write MultiCore icon");

    let resource_path = output_dir.join("multicore-desktop.rc");
    let manifest_name = match std::env::var("PROFILE").as_deref() {
        Ok("release") => "app.manifest",
        _ => "app-debug.manifest",
    };
    let manifest_path = manifest_dir.join(manifest_name);
    let mut resource_source = format!(
        "1 24 \"{}\"\n1 ICON \"{}\"\n",
        manifest_path.display().to_string().replace('\\', "/"),
        icon_path.display().to_string().replace('\\', "/"),
    );
    resource_source.push_str(&windows_version_info(
        &multicore_app_version(manifest_dir),
        "MULTICORE",
        "MultiCore.exe",
    ));
    std::fs::write(&resource_path, resource_source)
        .expect("failed to create MultiCore Windows resource source");
    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed MultiCore Windows resources");
}

/// The canonical logo with `badge` drawn in its empty lower-left corner, the logo itself kept
/// clear of the badge by a thin transparent gap so the badge reads on any taskbar colour.
fn tray_state_svg(badge: &str) -> String {
    let svg = std::str::from_utf8(APP_ICON_SVG).expect("the canonical SVG is UTF-8");
    let body_start = svg.find('>').expect("the canonical SVG has a root element") + 1;
    let body_end = svg.rfind("</svg>").expect("the canonical SVG is closed");
    let logo = &svg[body_start..body_end];
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="none">
<defs><mask id="badge-gap" maskUnits="userSpaceOnUse" x="0" y="0" width="64" height="64">
<rect width="64" height="64" fill="#fff"/><circle cx="13" cy="51" r="14.5" fill="#000"/>
</mask></defs>
<g mask="url(#badge-gap)">{logo}</g>
{badge}
</svg>"##
    )
}

fn rasterize_app_icon(tree: &resvg::usvg::Tree, size: u32) -> Vec<u8> {
    let source_size = tree.size();
    let scale = (size as f32 / source_size.width()).min(size as f32 / source_size.height());
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)
        .expect("MultiCore icon dimensions must be non-zero");
    resvg::render(
        tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    pixmap
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| {
            let alpha = pixel[3];
            if alpha == 0 {
                [0, 0, 0, 0]
            } else {
                [
                    unpremultiply(pixel[0], alpha),
                    unpremultiply(pixel[1], alpha),
                    unpremultiply(pixel[2], alpha),
                    alpha,
                ]
            }
        })
        .collect()
}

fn unpremultiply(channel: u8, alpha: u8) -> u8 {
    ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
}

/// Exposes the pinned Mihomo and Xray versions from the packaging manifest so
/// the settings footer cannot drift from what the release actually bundles.
fn emit_runtime_versions(manifest_dir: &std::path::Path) {
    let relative = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        "../../packaging/windows-x64/versions.json"
    } else {
        "../../packaging/linux/versions.json"
    };
    let versions_path = manifest_dir.join(relative);
    println!("cargo:rerun-if-changed={}", versions_path.display());
    let runtime_versions = std::fs::read_to_string(&versions_path)
        .ok()
        .and_then(|manifest| {
            let mihomo = pinned_component_version(&manifest, "mihomo")?;
            let xray = pinned_component_version(&manifest, "xray")?;
            Some(format!("MULTICORE · Mihomo {mihomo} · Xray {xray}"))
        })
        .unwrap_or_else(|| "MULTICORE".to_owned());
    println!("cargo:rustc-env=MULTICORE_RUNTIME_VERSIONS={runtime_versions}");
}

fn pinned_component_version(manifest: &str, component: &str) -> Option<String> {
    let start = manifest.find(&format!("\"{component}\": {{"))?;
    let rest = &manifest[start..];
    let key = "\"version\": \"";
    let value = &rest[rest.find(key)? + key.len()..];
    let version = &value[..value.find('"')?];
    Some(version.trim_start_matches('v').to_owned())
}

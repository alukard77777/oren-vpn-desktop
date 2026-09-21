const APP_ICON_SVG: &[u8] = include_bytes!("assets/app-icon.svg");
const TRAY_ICON_SIZE: u32 = 32;

fn main() {
    println!("cargo:rerun-if-env-changed=MULTICORE_UPDATE_REPOSITORY");
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let output_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    let icon_tree = resvg::usvg::Tree::from_data(APP_ICON_SVG, &resvg::usvg::Options::default())
        .expect("failed to parse the canonical MultiCore SVG icon");
    let tray_icon = rasterize_app_icon(&icon_tree, TRAY_ICON_SIZE);
    std::fs::write(output_dir.join("app-icon-32.rgba"), tray_icon)
        .expect("failed to write the generated MultiCore tray icon");

    let components_path = manifest_dir
        .join("ui/components.slint")
        .display()
        .to_string()
        .replace('\\', "/");
    let check_source = format!(
        r#"import {{ IconAction, PrimaryAction, QuietChip, RouteNodeRow, DetailsSurface }} from "{components_path}";
export component ComponentCheck inherits Window {{
    preferred-width: 480px;
    preferred-height: 720px;
    IconAction {{ state-label: "Открыть действие"; }}
    PrimaryAction {{ label: "Подключить"; state-label: "Подключить профиль"; }}
    QuietChip {{ label: "Автовыбор"; selected: true; }}
    RouteNodeRow {{ label: "Новосибирск"; trailing-text: "42 мс"; pending: true; }}
    DetailsSurface {{ title: "Диагностика"; }}
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
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app-debug.manifest");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        compile_windows_resources(&manifest_dir, &output_dir, &icon_tree);
    }

    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles),
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
    let resource_source = format!(
        "1 24 \"{}\"\n1 ICON \"{}\"\n",
        manifest_path.display().to_string().replace('\\', "/"),
        icon_path.display().to_string().replace('\\', "/"),
    );
    std::fs::write(&resource_path, resource_source)
        .expect("failed to create MultiCore Windows resource source");
    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed MultiCore Windows resources");
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

fn main() {
    #[cfg(windows)]
    embed_windows_icon();
}

// The path to the source favicon, relative to this crate's manifest. The .ico is
// rendered from the SVG so the logo has a single, scalable source of truth.
#[cfg(windows)]
const FAVICON_SVG_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/logo/constellation-favicon.svg",
);

// The icon sizes packed into the .ico, smallest to largest. Windows picks the
// best fit per context (16 in the title bar, 256 on the desktop). Each size is
// rendered from the SVG directly, so none is an upscale of another.
#[cfg(windows)]
const ICON_SIZES_PX: [u32; 5] = [16, 32, 48, 128, 256];

/// The favicon SVG rendered into an `.ico` and embedded in the executable.
#[cfg(windows)]
fn embed_windows_icon() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    let ico_path = std::path::Path::new(&out_dir).join("constellation-favicon.ico");

    write_icon(&ico_path);

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(ico_path.to_str().expect("OUT_DIR path is valid utf-8"));

    resource.compile().expect("embed the windows icon resource");

    println!("cargo:rerun-if-changed={FAVICON_SVG_PATH}");
    println!("cargo:rerun-if-changed=build.rs");
}

/// The favicon SVG rendered at every icon size and written as a multi-resolution `.ico`.
#[cfg(windows)]
fn write_icon(ico_path: &std::path::Path) {
    use resvg::{tiny_skia, usvg};

    let svg = std::fs::read_to_string(FAVICON_SVG_PATH).expect("read the favicon svg");

    let tree =
        usvg::Tree::from_str(&svg, &usvg::Options::default()).expect("parse the favicon svg");

    let intrinsic = tree.size().width();

    assert!(intrinsic > 0.0, "the svg must have a positive width");

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);

    for size in ICON_SIZES_PX {
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("allocate the render target");

        let scale = size as f32 / intrinsic;
        let transform = tiny_skia::Transform::from_scale(scale, scale);

        resvg::render(&tree, transform, &mut pixmap.as_mut());

        let rgba = straight_rgba(pixmap.pixels());
        let icon_image = ico::IconImage::from_rgba_data(size, size, rgba);
        let entry = ico::IconDirEntry::encode(&icon_image).expect("encode an ico entry");

        icon_dir.add_entry(entry);
    }

    assert_eq!(
        icon_dir.entries().len(),
        ICON_SIZES_PX.len(),
        "every icon size must be encoded",
    );

    let file = std::fs::File::create(ico_path).expect("create the ico file");
    icon_dir.write(file).expect("write the ico file");
}

/// The tiny-skia pixels un-premultiplied into the straight RGBA bytes `.ico` expects.
#[cfg(windows)]
fn straight_rgba(pixels: &[resvg::tiny_skia::PremultipliedColorU8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixels.len() * 4);

    for pixel in pixels {
        let color = pixel.demultiply();

        rgba.push(color.red());
        rgba.push(color.green());
        rgba.push(color.blue());
        rgba.push(color.alpha());
    }

    assert_eq!(rgba.len(), pixels.len() * 4, "each pixel yields four bytes");

    rgba
}

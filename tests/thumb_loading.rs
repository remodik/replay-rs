//! Регрессия: превью генерировались, но окно их не показывало.
//!
//! `egui_extras::install_image_loaders` ставит декодер картинок под фичей
//! `image`, а читать `file://` умеет отдельный `FileLoader` под фичей `file`.
//! С одной только `image` список клипов показывал на месте превью ошибку
//! «No matching BytesLoader».
//!
//! Тесты этого не видели: они проверяли, что JPEG создан и корректен, но
//! никогда — что его удаётся загрузить так, как это делает окно.

use eframe::egui;

/// Путь берём настоящий: загрузчику нужно что-то прочитать.
fn sample_jpeg() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("replay-thumb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("thumb.jpg");
    // Минимальный валидный JPEG 1x1.
    const JPEG_1X1: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xC9, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xCC,
        0x00, 0x06, 0x00, 0x10, 0x10, 0x05, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F,
        0x00, 0xD2, 0xCF, 0x20, 0xFF, 0xD9,
    ];
    std::fs::write(&path, JPEG_1X1).unwrap();
    path
}

#[test]
fn file_uri_has_a_bytes_loader() {
    let path = sample_jpeg();
    let ctx = egui::Context::default();
    egui_extras::install_image_loaders(&ctx);

    // Ровно тот же URI, что строит список клипов.
    let uri = format!("file://{}", path.display());
    let result = ctx.try_load_bytes(&uri);

    assert!(
        !matches!(result, Err(egui::load::LoadError::NoMatchingBytesLoader)),
        "для {uri} нет загрузчика: у egui_extras не включена фича \"file\", \
         и окно покажет ошибку вместо превью"
    );

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

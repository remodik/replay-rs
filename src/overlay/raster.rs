//! Программная отрисовка вывода egui в буфер ARGB.
//!
//! egui отдаёт треугольники с цветом вершин (sRGB, premultiplied alpha) и
//! координатами в атласе текстур. Здесь они закрашиваются по пикселям:
//! барицентрические координаты, выборка из текстуры ближайшим соседом,
//! смешивание «source over» для premultiplied alpha. Сглаживание краёв
//! egui делает сам — полупрозрачной «бахромой» в вершинах.
//!
//! Формат пикселя — `0xAARRGGBB` с premultiplied alpha: ровно то, что ждут
//! `wl_shm` (ARGB8888) и 32-битный DIB для `UpdateLayeredWindow`.

use std::collections::HashMap;

use eframe::egui;
use egui::epaint::{ClippedPrimitive, Primitive};

/// Буфер кадра.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width * height],
        }
    }

    /// Умножает всю картинку на непрозрачность (premultiplied — все каналы).
    pub fn fade(&mut self, opacity: f32) {
        if opacity >= 1.0 {
            return;
        }
        let k = opacity.clamp(0.0, 1.0);
        let scale = |c: u32| ((c as f32) * k + 0.5) as u32;
        for p in &mut self.pixels {
            let a = scale(*p >> 24);
            let r = scale((*p >> 16) & 0xff).min(a);
            let g = scale((*p >> 8) & 0xff).min(a);
            let b = scale(*p & 0xff).min(a);
            *p = (a << 24) | (r << 16) | (g << 8) | b;
        }
    }

    /// Байты в порядке памяти little-endian ARGB8888 (B, G, R, A).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.pixels.iter().flat_map(|p| p.to_le_bytes()).collect()
    }

    /// JPEG поверх тёмно-серого фона — посмотреть глазами в отладке.
    pub fn save_jpeg(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let mut rgb = Vec::with_capacity(self.width * self.height * 3);
        for p in &self.pixels {
            let a = p >> 24;
            for shift in [16, 8, 0] {
                let c = (p >> shift) & 0xff;
                rgb.push((c + (60 * (255 - a)) / 255) as u8);
            }
        }
        image::save_buffer(
            path,
            &rgb,
            self.width as u32,
            self.height as u32,
            image::ColorType::Rgb8,
        )?;
        Ok(())
    }
}

/// Текстура в памяти: цвета egui (premultiplied).
struct Texture {
    width: usize,
    height: usize,
    pixels: Vec<egui::Color32>,
}

/// Хранит текстуры egui и рисует примитивы.
#[derive(Default)]
pub struct SoftPainter {
    textures: HashMap<egui::TextureId, Texture>,
}

impl SoftPainter {
    pub fn update_textures(&mut self, delta: &egui::TexturesDelta) {
        for (id, d) in delta
            .set
            .iter()
            .flat_map(|(id, ds)| ds.iter().map(move |d| (id, d)))
        {
            let egui::ImageData::Color(img) = &d.image;
            let (w, h) = (img.size[0], img.size[1]);
            match d.pos {
                None => {
                    self.textures.insert(
                        *id,
                        Texture {
                            width: w,
                            height: h,
                            pixels: img.pixels.clone(),
                        },
                    );
                }
                Some([x0, y0]) => {
                    if let Some(t) = self.textures.get_mut(id) {
                        for y in 0..h {
                            for x in 0..w {
                                let (tx, ty) = (x0 + x, y0 + y);
                                if tx < t.width && ty < t.height {
                                    t.pixels[ty * t.width + tx] = img.pixels[y * w + x];
                                }
                            }
                        }
                    }
                }
            }
        }
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    pub fn paint(&self, canvas: &mut Canvas, prims: &[ClippedPrimitive], ppp: f32) {
        for p in prims {
            let Primitive::Mesh(mesh) = &p.primitive else {
                continue;
            };
            let Some(tex) = self.textures.get(&mesh.texture_id) else {
                continue;
            };
            let clip = (
                ((p.clip_rect.min.x * ppp).floor().max(0.0)) as i64,
                ((p.clip_rect.min.y * ppp).floor().max(0.0)) as i64,
                ((p.clip_rect.max.x * ppp).ceil().min(canvas.width as f32)) as i64,
                ((p.clip_rect.max.y * ppp).ceil().min(canvas.height as f32)) as i64,
            );
            for tri in mesh.indices.as_chunks::<3>().0 {
                let v = [
                    &mesh.vertices[tri[0] as usize],
                    &mesh.vertices[tri[1] as usize],
                    &mesh.vertices[tri[2] as usize],
                ];
                triangle(canvas, tex, &v, ppp, clip);
            }
        }
    }
}

fn triangle(
    canvas: &mut Canvas,
    tex: &Texture,
    v: &[&egui::epaint::Vertex; 3],
    ppp: f32,
    clip: (i64, i64, i64, i64),
) {
    let p: [(f32, f32); 3] = [
        (v[0].pos.x * ppp, v[0].pos.y * ppp),
        (v[1].pos.x * ppp, v[1].pos.y * ppp),
        (v[2].pos.x * ppp, v[2].pos.y * ppp),
    ];
    let area = edge(p[0], p[1], p[2]);
    if area.abs() < 1e-6 {
        return;
    }
    let min_x = (p.iter().map(|q| q.0).fold(f32::MAX, f32::min).floor() as i64).max(clip.0);
    let max_x = (p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil() as i64).min(clip.2);
    let min_y = (p.iter().map(|q| q.1).fold(f32::MAX, f32::min).floor() as i64).max(clip.1);
    let max_y = (p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as i64).min(clip.3);
    let col: [[f32; 4]; 3] = [rgba(v[0].color), rgba(v[1].color), rgba(v[2].color)];
    for y in min_y..max_y {
        for x in min_x..max_x {
            let c = (x as f32 + 0.5, y as f32 + 0.5);
            let w0 = edge(p[1], p[2], c) / area;
            let w1 = edge(p[2], p[0], c) / area;
            let w2 = edge(p[0], p[1], c) / area;
            // Включаем рёбра с небольшим допуском — без щелей между
            // соседними треугольниками.
            if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                continue;
            }
            let u = w0 * v[0].uv.x + w1 * v[1].uv.x + w2 * v[2].uv.x;
            let t = w0 * v[0].uv.y + w1 * v[1].uv.y + w2 * v[2].uv.y;
            let tx = ((u * tex.width as f32) as usize).min(tex.width - 1);
            let ty = ((t * tex.height as f32) as usize).min(tex.height - 1);
            let s = rgba(tex.pixels[ty * tex.width + tx]);
            let mut src = [0f32; 4];
            for (i, out) in src.iter_mut().enumerate() {
                let vc = w0 * col[0][i] + w1 * col[1][i] + w2 * col[2][i];
                *out = vc * s[i];
            }
            let idx = y as usize * canvas.width + x as usize;
            canvas.pixels[idx] = over(src, canvas.pixels[idx]);
        }
    }
}

fn edge(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

/// Цвет egui (premultiplied) в долях единицы: [r, g, b, a].
fn rgba(c: egui::Color32) -> [f32; 4] {
    let [r, g, b, a] = c.to_array();
    [
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ]
}

/// «Source over» для premultiplied alpha.
fn over(src: [f32; 4], dst: u32) -> u32 {
    let inv = 1.0 - src[3].clamp(0.0, 1.0);
    let d = [
        ((dst >> 16) & 0xff) as f32 / 255.0,
        ((dst >> 8) & 0xff) as f32 / 255.0,
        (dst & 0xff) as f32 / 255.0,
        (dst >> 24) as f32 / 255.0,
    ];
    let ch = |s: f32, d: f32| (((s + d * inv).clamp(0.0, 1.0)) * 255.0 + 0.5) as u32;
    let r = ch(src[0], d[0]);
    let g = ch(src[1], d[1]);
    let b = ch(src[2], d[2]);
    let a = ch(src[3], d[3]);
    (a << 24) | (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_over_premultiplied() {
        // Непрозрачный красный поверх чего угодно — красный.
        assert_eq!(over([1.0, 0.0, 0.0, 1.0], 0xff00ff00), 0xffff0000);
        // Прозрачный источник не меняет фон.
        assert_eq!(over([0.0, 0.0, 0.0, 0.0], 0x80102030), 0x80102030);
        // Половина белого поверх пустоты — полупрозрачный белый.
        let p = over([0.5, 0.5, 0.5, 0.5], 0);
        assert_eq!(p >> 24, 128);
    }

    #[test]
    fn fade_keeps_premultiplication() {
        let mut c = Canvas::new(1, 1);
        c.pixels[0] = 0xff808080;
        c.fade(0.5);
        let p = c.pixels[0];
        assert_eq!(p >> 24, 0x80);
        assert!(
            ((p >> 16) & 0xff) <= (p >> 24),
            "цвет больше альфы: {p:08x}"
        );
    }
}

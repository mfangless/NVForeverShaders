//! Diagnostic survey of hair-shader shapes' open borders and draw modes, for softening the
//! hairline (docs/design/graphics-enhanced.md §3.26 "Hair edges"; docs/spec-notes/nif.md §6
//! "Hair borders and alpha"). Per shape with the Hair shader flag (BSShaderFlags bit 18
//! [documented: nif.xml BSShaderFlags]) in a mesh whose path holds "hair": its vertices
//! welded by position (a UV seam is no border), the edges used by one triangle only (the
//! open border), where the border's vertices lie in the shape's bounds, the diffuse
//! texture's alpha at them against the alpha at every vertex, and the draw mode its
//! NiAlphaProperty gets (nvf-assets nif_spawn.rs `alpha_mode`).
//!
//! cargo run --release -p fnv-nif --example hair_border_survey -- <schema dir> <meshes.bsa>... --textures <textures.bsa>... [--list]

use fnv_dds::Dds;
use fnv_nif::scene::{mesh_data, scene_objects, Decoded};
use fnv_nif::{NifFile, Schema};
use std::collections::HashMap;

struct Tex {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

impl Tex {
    fn alpha(&self, uv: [f32; 2]) -> f32 {
        let x = ((uv[0].rem_euclid(1.0)) * self.w as f32) as u32;
        let y = ((uv[1].rem_euclid(1.0)) * self.h as f32) as u32;
        let i = (y.min(self.h - 1) * self.w + x.min(self.w - 1)) as usize;
        self.rgba[i * 4 + 3] as f32 / 255.0
    }
}

fn load_texture(archives: &mut [fnv_bsa::Archive], path: &str) -> Option<Tex> {
    let p = path.to_ascii_lowercase().replace('/', "\\");
    let p = if p.starts_with("textures\\") { p } else { format!("textures\\{p}") };
    for a in archives.iter_mut() {
        let Some((fi, ri)) = a.find(&p) else { continue };
        let rec = a.folders[fi].files[ri].clone();
        let data = a.extract(&rec).ok()?;
        let dds = Dds::parse(&data).ok()?;
        let (w, h) = (dds.header.width, dds.header.height);
        let top = dds.levels[0][0];
        let rgba = fnv_dds::decode_blocks(dds.format, w, h, top).or_else(|| fnv_dds::to_rgba8(dds.format, top))?;
        return Some(Tex { w, h, rgba });
    }
    None
}

/// The engine's draw mode for NiAlphaProperty flags and threshold (nvf-assets nif_spawn.rs
/// `alpha_mode`, BLEND_TEST_CUT 64), as a label.
fn mode(alpha: Option<(i64, i64)>) -> String {
    let Some((flags, threshold)) = alpha else { return "no alpha property (opaque)".into() };
    let (blend, test) = (flags & 1 != 0, flags & 0x200 != 0);
    let dst = (flags >> 5) & 0xF;
    match (blend, test) {
        (true, _) if dst == 0 => "additive".into(),
        (false, true) => format!("alpha test at {threshold}"),
        (true, true) if threshold >= 64 => format!("blend+test, drawn tested at {threshold}"),
        (true, true) => format!("blend+test, drawn blended (test {threshold})"),
        (true, false) => "blended".into(),
        _ => "opaque".into(),
    }
}

fn main() {
    let mut a: Vec<String> = std::env::args().collect();
    let list = a.iter().position(|x| x == "--list").map(|i| a.remove(i)).is_some();
    let t = a.iter().position(|x| x == "--textures").expect("--textures");
    let textures: Vec<String> = a.drain(t..).skip(1).collect();
    let schema = Schema::load_dir(std::path::Path::new(&a[1])).unwrap();
    let mut tex_arcs: Vec<fnv_bsa::Archive> = textures.iter().filter_map(|p| fnv_bsa::Archive::open(p).ok()).collect();
    let (mut shapes, mut open, mut closed) = (0, 0, 0);
    let (mut border_alpha, mut all_alpha, mut front_alpha) = (Vec::new(), Vec::new(), Vec::new());
    let mut modes: std::collections::BTreeMap<String, usize> = Default::default();
    for arc in &a[2..] {
        let mut bsa = fnv_bsa::Archive::open(arc).unwrap();
        let entries: Vec<(String, fnv_bsa::FileRecord)> = bsa
            .entries()
            .map(|e| (e.path(), e.record.clone()))
            .filter(|(p, _)| p.ends_with(".nif") && p.contains("hair"))
            .collect();
        for (path, rec) in entries {
            let Ok(data) = bsa.extract(&rec) else { continue };
            let Ok(file) = NifFile::parse(&data, Some(&schema)) else { continue };
            let dec = Decoded::new(&file, &data, &schema);
            for o in scene_objects(&file, &dec).iter().filter(|o| !o.is_node) {
                let Ok(shape) = &dec.blocks[o.block] else { continue };
                let mut hair = None;
                let mut alpha = None;
                for &r in shape.refs("properties").unwrap_or_default() {
                    let Ok(r) = usize::try_from(r) else { continue };
                    let Some(s) = dec.blocks.get(r).and_then(|x| x.as_ref().ok()) else { continue };
                    if file.blocks[r].type_name == "NiAlphaProperty" {
                        alpha = Some((s.int("flags").unwrap_or(0), s.int("threshold").unwrap_or(128)));
                    }
                    if file.blocks[r].type_name == "BSShaderPPLightingProperty" && s.int("shader_flags").unwrap_or(0) & (1 << 18) != 0 {
                        hair = Some(
                            s.block_ref("texture_set")
                                .and_then(|t| dec.get(t))
                                .and_then(|t| t.list("textures"))
                                .and_then(|l| match l.first() {
                                    Some(fnv_nif::Value::Str { text: Some(t), .. }) => Some(t.clone()),
                                    _ => None,
                                })
                                .unwrap_or_default(),
                        );
                    }
                }
                let Some(texture) = hair else { continue };
                let Some(md) = shape.block_ref("data").and_then(|r| dec.get(r)).map(mesh_data) else { continue };
                if md.positions.is_empty() || md.triangles.is_empty() {
                    continue;
                }
                shapes += 1;
                *modes.entry(mode(alpha)).or_default() += 1;
                // Weld by position (0.01 units).
                let mut ids: HashMap<[i64; 3], u32> = HashMap::new();
                let weld: Vec<u32> = md
                    .positions
                    .iter()
                    .map(|p| {
                        let k = p.map(|c| (c * 100.0).round() as i64);
                        let n = ids.len() as u32;
                        *ids.entry(k).or_insert(n)
                    })
                    .collect();
                let mut edges: HashMap<(u32, u32), u32> = HashMap::new();
                for t in &md.triangles {
                    let w = t.map(|i| weld[i as usize]);
                    for (x, y) in [(w[0], w[1]), (w[1], w[2]), (w[2], w[0])] {
                        if x != y {
                            *edges.entry((x.min(y), x.max(y))).or_default() += 1;
                        }
                    }
                }
                let border: std::collections::HashSet<u32> =
                    edges.iter().filter(|(_, &c)| c == 1).flat_map(|(&(x, y), _)| [x, y]).collect();
                if border.is_empty() {
                    closed += 1;
                } else {
                    open += 1;
                }
                let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                for p in &md.positions {
                    for k in 0..3 {
                        lo[k] = lo[k].min(p[k]);
                        hi[k] = hi[k].max(p[k]);
                    }
                }
                let tex = load_texture(&mut tex_arcs, &texture);
                let uvs = md.uv_sets.first();
                // "Front": the forward quarter of the bounds along +Y (the head's gaze, as the
                // eye meshes look along +Y [observed: nif_material.wesl EYE notes]) and the
                // upper half along +Z.
                let front = |p: [f32; 3]| p[1] > hi[1] - 0.25 * (hi[1] - lo[1]) && p[2] > lo[2] + 0.5 * (hi[2] - lo[2]);
                let (mut nb, mut nf, mut sb, mut sf) = (0usize, 0usize, 0f32, 0f32);
                for (i, p) in md.positions.iter().enumerate() {
                    let al = match (&tex, uvs) {
                        (Some(t), Some(u)) if i < u.len() => Some(t.alpha(u[i])),
                        _ => None,
                    };
                    if let Some(al) = al {
                        all_alpha.push(al);
                    }
                    if border.contains(&weld[i]) {
                        nb += 1;
                        if let Some(al) = al {
                            border_alpha.push(al);
                            sb += al;
                        }
                        if front(*p) {
                            nf += 1;
                            if let Some(al) = al {
                                front_alpha.push(al);
                                sf += al;
                            }
                        }
                    }
                }
                if list {
                    println!(
                        "{path} :: {:?} :: {texture} | verts {} welded {} tris {} border edges {} border verts {} (front-top {}) | alpha at border {:.2}, front-top {:.2}{}",
                        o.name,
                        md.positions.len(),
                        ids.len(),
                        md.triangles.len(),
                        edges.values().filter(|&&c| c == 1).count(),
                        nb,
                        nf,
                        sb / nb.max(1) as f32,
                        sf / nf.max(1) as f32,
                        if tex.is_none() { " (texture not found)" } else { "" }
                    );
                }
            }
        }
    }
    let q = |v: &mut Vec<f32>, f: f32| {
        v.sort_by(|a, b| a.total_cmp(b));
        v.get(((v.len() as f32 - 1.0) * f).round() as usize).copied().unwrap_or(0.0)
    };
    println!("hair-shader shapes {shapes}: with an open border {open}, closed {closed}");
    println!("draw modes: {modes:#?}");
    for (name, v) in [("every vertex", &mut all_alpha), ("border vertices", &mut border_alpha), ("front-top border", &mut front_alpha)] {
        let n = v.len();
        let hi = v.iter().filter(|&&x| x >= 0.8).count();
        println!(
            "alpha at {name} ({n}): 10/50/90th percentiles {:.2} / {:.2} / {:.2}; at or above 0.8 (HAIR_MODEL_CUT) {:.0}%",
            q(v, 0.1),
            q(v, 0.5),
            q(v, 0.9),
            100.0 * hi as f32 / n.max(1) as f32
        );
    }
}

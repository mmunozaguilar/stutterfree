//! Static engine readers and the planner (SCSKiller.Core's readers + Planning), ported one engine at a time.
//!
//! Ported: Unity (DirectX 11), Unreal 4/5 (DirectX 11; DirectX 12 through plan12: stock root signatures rebuilt from the
//! shaders' counts, neutral-state pipelines) and FromSoftware's Dark Souls III / Remastered (DirectX 11). DirectX 11 shaders
//! become '1' items the warmer draws / dispatches once, hull + domain pairs '2' items. Not yet: Elden Ring and the other
//! engines' readers; those games compile from a Record-mode recording.

pub mod d3d12;
pub mod fromsoft;
pub mod plan12;
pub mod unity;
pub mod unreal;

use crate::dxbc;
use crate::models::GameInfo;
use crate::psodb;
use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

/// What a plan produced: the items written to the work folder's shaderless_gen.db, whether part of it was guessed (a DX12
/// plan without a usable recording: the card shows the "Partial" note), and a one-line summary.
pub struct PlanOutcome {
    pub planned_items: usize,
    pub partial: bool,
    pub summary: String,
    /// what the card shows: the engine and graphics API the plan is for ("Unreal 5.5 D3D12")
    pub label: String,
}

/// Plans the compile from the game's own files into `work_dir`/shaderless_gen.db.
pub fn plan_from_game_files(game: &GameInfo, work_dir: &Path, _recording: Option<&Path>, cancel: &crate::proton::CancelToken) -> Result<PlanOutcome, String> {
    if let Some((_, data)) = unity::find(&game.install_path, &game.executable_path) {
        let api = unity::graphics_api(&data, game.prefix_path.as_deref());
        if !api.starts_with("D3D11") {
            return Err(format!("this Unity game runs {api}: its DirectX 12 pipelines can't be planned from the game files yet"));
        }
        let ix = unity::index_d3d11(&data, cancel);
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let what = format!("Unity {api}: {} files, {} shader objects", ix.files, ix.shader_objects);
        let label = format!("Unity {}", api.split(' ').next().unwrap_or(&api));
        return write_d3d11_plan(&ix.shaders, &[], &work_dir.join("shaderless_gen.db"), what).map(|o| PlanOutcome { label, ..o });
    }
    if let Some(title) = fromsoft::title_of(Path::new(&game.executable_path)) {
        let key_file = crate::recorder::game_dir(&game.id).join("archive.keys");
        let ix = fromsoft::index(title, Path::new(&game.executable_path), &key_file, cancel)?;
        let what = format!("{} {}: {} shader files", title.name, title.api, ix.files);
        let out = work_dir.join("shaderless_gen.db");
        let label = format!("{} {}", title.name, title.api);
        if title.api == "D3D11" {
            let shaders = ix.shaders.into_iter().filter_map(|(h, c)| dxbc::is_d3d11(&c).map(|st| (h, (st, c)))).collect();
            return write_d3d11_plan(&shaders, &ix.maps, &out, what).map(|o| PlanOutcome { label, ..o });
        }
        let set: HashMap<String, unreal::Shader> = ix
            .shaders
            .into_iter()
            .filter_map(|(h, c)| d3d12::stage(&c).map(|stage| (h, unreal::Shader { stage, stage11: 0, counts: Default::default(), container: c })))
            .collect();
        let maps: Vec<unreal::ShaderMap> = ix.maps.into_iter().map(|shas| unreal::ShaderMap { pooled: false, shas }).collect();
        return plan12::write(&set, &maps, plan12::RootSigs::Embedded, false, gpu_lanes(), &out, what).map(|o| PlanOutcome { label, ..o });
    }
    if unreal::is_unreal(Path::new(&game.install_path)) {
        return plan_unreal(game, work_dir, cancel);
    }
    Err(format!("reading {} shader archives from the game files is not ported to StutterFree yet", engine_label(game)))
}

/// The hull -> domain shader pairs of each map whose control points link (Planner.TessPairs): the HS's output signature
/// is the DS's input signature, register for register, every DS input written with a superset mask. Greedy: a pair is
/// kept when it brings a shader no kept pair has (the warm needs each HS and DS drawn once, not every combination).
/// shortcut: the tessellator domain and control-point counts aren't checked here; the warmer skips a mismatched pair.
fn tess_pairs<'a>(shaders: &'a HashMap<String, (u32, Vec<u8>)>, maps: &'a [Vec<String>]) -> Vec<(&'a str, &'a str)> {
    let mut refl: HashMap<&str, d3d12::Reflection> = HashMap::new();
    let mut paired: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for map in maps {
        let of = |stage: u32| map.iter().filter(move |h| shaders.get(*h).map(|s| s.0 == stage).unwrap_or(false)).map(String::as_str);
        for h in of(d3d12::HS).chain(of(d3d12::DS)) {
            refl.entry(h).or_insert_with(|| d3d12::reflect(&shaders[h].1));
        }
        let mut dss: HashMap<String, Vec<&str>> = HashMap::new();
        for d in of(d3d12::DS) {
            dss.entry(d3d12::sig_key(&refl[d].inputs, true)).or_default().push(d);
        }
        for h in of(d3d12::HS) {
            for &d in dss.get(&d3d12::sig_key(&refl[h].outputs, true)).into_iter().flatten() {
                // control points: every DS input, system values included, comes from the HS
                let links = refl[d].inputs.iter().all(|i| refl[h].outputs.iter().any(|o| o.name == i.name && o.index == i.index && i.mask & !o.mask == 0));
                if links && (paired.insert(h) | paired.insert(d)) {
                    out.push((h, d));
                }
            }
        }
    }
    out
}

/// 'B' blobs, one '1' item per shader, and one '2' item per hull + domain pair of a map (`maps`; none: HS and DS left out).
fn write_d3d11_plan(shaders: &HashMap<String, (u32, Vec<u8>)>, maps: &[Vec<String>], out: &Path, what: String) -> Result<PlanOutcome, String> {
    let mut items: Vec<(&String, u32)> = shaders.iter().filter(|(_, (st, _))| *st != d3d12::DS && *st != d3d12::HS).map(|(h, (st, _))| (h, *st)).collect();
    items.sort();
    if items.is_empty() {
        return Err(format!("no DirectX 11 shaders found ({what})"));
    }
    let pairs = tess_pairs(shaders, maps);
    let mut pair_blobs: Vec<&str> = pairs.iter().flat_map(|(h, d)| [*h, *d]).collect();
    pair_blobs.sort();
    pair_blobs.dedup();
    let res = (|| -> std::io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(out)?);
        for h in items.iter().map(|(h, _)| h.as_str()).chain(pair_blobs.iter().copied()) {
            psodb::write_blob(&mut w, h, &shaders[h].1)?;
        }
        for (h, st) in &items {
            let mut p = st.to_le_bytes().to_vec();
            p.extend(hex::decode(h).map_err(|e| std::io::Error::other(e.to_string()))?);
            psodb::write(&mut w, b'1', &p)?;
        }
        for (h, d) in &pairs {
            let mut p = hex::decode(h).map_err(|e| std::io::Error::other(e.to_string()))?;
            p.extend(hex::decode(d).map_err(|e| std::io::Error::other(e.to_string()))?);
            psodb::write(&mut w, b'2', &p)?;
        }
        w.flush()
    })();
    res.map_err(|e| format!("couldn't write the plan: {e}"))?;
    let unpaired = shaders.len() - items.len() - pair_blobs.len();
    Ok(PlanOutcome {
        planned_items: items.len() + pairs.len(),
        partial: false,
        label: String::new(),
        summary: format!(
            "{what} -> {} DirectX 11 shaders{}{}",
            items.len(),
            if pairs.is_empty() { String::new() } else { format!(" + {} tessellation pairs", pairs.len()) },
            if unpaired > 0 { format!(" ({unpaired} tessellation shaders left out)") } else { String::new() }
        ),
    })
}

/// Unreal: DX11 games get their SM5 shaders planned like Unity's; DX12 games their compute and VS / VS+PS pipelines
/// (plan12). shortcut: a game that may run on either API gets the DX12 plan only.
fn plan_unreal(game: &GameInfo, work_dir: &Path, cancel: &crate::proton::CancelToken) -> Result<PlanOutcome, String> {
    let ix = unreal::index(Path::new(&game.install_path), Path::new(&game.executable_path), None);
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let counts: Vec<String> = { let mut v: Vec<_> = ix.shaders.iter().map(|(p, s)| format!("{p} {}", s.len())).collect(); v.sort(); v };
    if ix.libraries.is_empty() {
        let why = if ix.encrypted { "its containers are encrypted (StutterFree doesn't take AES keys yet)" } else { "no shader library was found in its .pak / .utoc containers (custom or unsupported container format)" };
        return Err(format!("Unreal {}: {why}", ix.project));
    }
    let api = unreal::graphics_api(&ix, game.prefix_path.as_deref());
    let (major, minor) = ix.version;
    let what = format!("Unreal {major}.{minor}{} {} {api}: {} shader libraries ({})", if ix.version_guessed { "?" } else { "" }, ix.project, ix.libraries.len(), counts.join(", "));
    let out = work_dir.join("shaderless_gen.db");
    let label = |api: &str| format!("Unreal {major}.{minor} {api}");
    let platform = |p: &str| ix.shaders.iter().find(|(k, _)| k.eq_ignore_ascii_case(p));
    if api.starts_with("D3D11") && !api.contains("D3D12") {
        let sm5: HashMap<String, (u32, Vec<u8>)> = platform("PCD3D_SM5")
            .map(|(_, set)| set.iter().filter(|(_, s)| s.stage11 != 0).map(|(h, s)| (h.clone(), (s.stage11, s.container.clone()))).collect())
            .unwrap_or_default();
        let maps: Vec<Vec<String>> = ix.maps.get("PCD3D_SM5").map(|m| m.iter().map(|m| m.shas.clone()).collect()).unwrap_or_default();
        return write_d3d11_plan(&sm5, &maps, &out, what).map(|o| PlanOutcome { label: label("D3D11"), ..o });
    }
    if api.starts_with("Vulkan") {
        return Err(format!("{what}: Vulkan games aren't planned"));
    }
    // the DX12 RHI runs SM6 shaders where the game ships them (every GPU vkd3d-proton runs on supports SM 6)
    let Some((plat, set)) = platform("PCD3D_SM6").or_else(|| platform("PCD3D_SM5")) else { return Err(format!("{what}: no D3D12 shader platform")) };
    let Some(rule) = d3d12::rule_for(major, minor) else { return Err(format!("{what}: no root signature rule for this Unreal version")) };
    let maps = ix.maps.get(plat.as_str()).map(Vec::as_slice).unwrap_or(&[]);
    let rs = plan12::RootSigs::Unreal { rule, mesh_tier: plat.eq_ignore_ascii_case("PCD3D_SM6") };
    plan12::write(set, maps, rs, ix.ue5, gpu_lanes(), &out, format!("{what}, {plat}, rule {rule:?}")).map(|o| PlanOutcome { label: label("D3D12"), ..o })
}

/// The wave sizes the GPU runs: NVIDIA 32; AMD 32 and 64. shortcut: Intel's range (8-32) is assumed, no Intel GPU tested.
fn gpu_lanes() -> (u32, u32) {
    match crate::drivers::detect_active_backend().vendor {
        crate::models::GpuVendor::Nvidia => (32, 32),
        crate::models::GpuVendor::Intel => (8, 32),
        _ => (32, 64),
    }
}

fn engine_label(game: &GameInfo) -> &'static str {
    use crate::models::GameEngine::*;
    match game.engine {
        UnrealEngine4 | UnrealEngine5 => "Unreal Engine",
        Unity => "Unity",
        RedEngine => "REDengine",
        FromSoftware => "FromSoftware",
        ReEngine => "RE Engine",
        Northlight => "Northlight",
        Dagor => "Dagor",
        DirectX12 | DirectX11 | Unknown => "this engine's",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d3d11_plan_references_its_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = unity::D3D11Index::default();
        for (kind, stage) in [(0u32, 2u32), (1, 1), (3, 4)] {
            let c = crate::dxbc::test_container(kind, 5, 0);
            ix.shaders.insert(hex::encode(<sha1::Sha1 as sha1::Digest>::digest(&c)), (stage, c));
        }
        let out = dir.path().join("gen.db");
        let o = write_d3d11_plan(&ix.shaders, &[], &out, "test".into()).unwrap();
        assert_eq!(o.planned_items, 2); // the hull shader waits for pairing
        let recs = psodb::read_all(&out).unwrap();
        let blobs: std::collections::HashSet<String> = recs.iter().filter_map(|r| (r.tag == b'B').then(|| r.blob_hash().unwrap())).collect();
        let refs = psodb::references(recs.iter().filter(|r| r.tag == b'1')).unwrap();
        assert!(refs.is_subset(&blobs) && refs.len() == 2);
    }

    #[test]
    fn tessellation_pairs_of_a_map() {
        use crate::engines::d3d12::test_shader;
        let cp = ("POSITION", 0, 0, 0xF, 0);
        let hs = test_shader(3, &[cp], &[cp]);
        let ds = test_shader(4, &[cp], &[("SV_POSITION", 0, 0, 0xF, 1)]);
        let ds_other = test_shader(4, &[("TEXCOORD", 0, 0, 0xF, 0)], &[("SV_POSITION", 0, 0, 0xF, 1)]);
        let vs = test_shader(1, &[cp], &[cp]);
        let mut shaders = HashMap::new();
        let mut map = Vec::new();
        for (c, stage) in [(hs, 4u32), (ds, 3), (ds_other, 3), (vs, 1)] {
            let h = hex::encode(<sha1::Sha1 as sha1::Digest>::digest(&c));
            map.push(h.clone());
            shaders.insert(h, (stage, c));
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("gen.db");
        let o = write_d3d11_plan(&shaders, &[map.clone()], &out, "test".into()).unwrap();
        assert_eq!(o.planned_items, 2, "{}", o.summary); // the VS, the HS + its DS; the TEXCOORD DS doesn't link
        let recs = psodb::read_all(&out).unwrap();
        let pair = recs.iter().find(|r| r.tag == b'2').unwrap();
        assert_eq!(pair.payload, [hex::decode(&map[0]).unwrap(), hex::decode(&map[1]).unwrap()].concat());
        let blobs: std::collections::HashSet<String> = recs.iter().filter_map(|r| (r.tag == b'B').then(|| r.blob_hash().unwrap())).collect();
        assert!(blobs.contains(&map[0]) && blobs.contains(&map[1]) && !blobs.contains(&map[2]));
        assert!(o.summary.contains("1 tessellation shaders left out"), "{}", o.summary);
    }
}

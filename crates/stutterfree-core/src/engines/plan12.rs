//! The D3D12 plan of an Unreal game from its files (Planning/PlanBuilder.cs, the synthesized path): every compute shader,
//! and per shader map every VS alone (depth passes) and every VS->PS pair whose signatures link, each over the stock root
//! signature rebuilt from its stages' counts. The first pipeline of a shape (stages, render targets, depth) is written as a
//! neutral-state 'S' stream, the rest as 'P' items swapping its root signature, shaders and input layout.
//! Also VS->GS(->PS) chains, global VS->GS chains feeding material PSs, a material PS no VS of its map feeds behind the
//! global VSs (Nanite material passes), and (UE5) every VS with the PSs shared by many maps (a default material's).
//! shortcut: tessellation and mesh / amplification stage sets aren't planned (UE5 dropped tessellation; no tested game
//! ships mesh shaders), nor the per-stage-cache orphan pairing (AMD only).

use super::d3d12::{self, Desc, Reflection, Rule, CS, GS, PS, VS};
use super::unreal::{Shader, ShaderMap};
use super::PlanOutcome;
use crate::psodb::{self, Rec};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::Path;

/// Where a pipeline's root signature comes from.
pub enum RootSigs {
    /// Unreal's stock rule rebuilt from the stages' resource counts; `mesh_tier`: the RHI runs SM6 (denies mesh stages)
    Unreal { rule: Rule, mesh_tier: bool },
    /// the one every stage carries (RTS0), created by the game at version 1.0 (FromSoftware: Elden Ring)
    Embedded,
}

struct Builder<'a> {
    set: &'a HashMap<String, Shader>,
    refl: &'a HashMap<&'a str, Reflection>,
    rs_mode: &'a RootSigs,
    max_srv: u32,
    samplers: Vec<u8>,
    rs_of: HashMap<Desc, String>,
    /// embedded RTS0 part (sha1) -> its 1.0 root signature's sha1, None when it has no 1.0 form
    embedded_of: HashMap<String, Option<String>>,
    rs_blobs: Vec<(String, Vec<u8>)>,
    templates: HashMap<String, String>,
    seen: HashSet<Vec<(u32, &'a str)>>,
    used: HashSet<&'a str>,
    streams: Vec<Rec>,
    items: Vec<Rec>,
    stats: BTreeMap<&'static str, usize>,
}

impl<'a> Builder<'a> {
    /// The stage set's root signature (its sha1, the blob kept in `rs_blobs`), or why it has none.
    fn root_sig(&mut self, stages: &[(u32, &'a str)], vs: Option<&Reflection>) -> Result<String, &'static str> {
        let rs_blobs = &mut self.rs_blobs;
        let mut keep = |b: Vec<u8>| {
            let h = psodb::hex(&<sha1::Sha1 as sha1::Digest>::digest(&b));
            rs_blobs.push((h.clone(), b));
            h
        };
        match *self.rs_mode {
            RootSigs::Unreal { rule, mesh_tier } => {
                let counts: Vec<(u32, d3d12::Counts)> = stages.iter().map(|(s, h)| (*s, self.set[*h].counts)).collect();
                let desc = d3d12::build_stock(rule, &counts, mesh_tier, self.max_srv, vs.map(|v| v.inputs.iter().any(|i| i.sys == 0)).unwrap_or(false));
                if !stages.iter().all(|(s, h)| d3d12::covers(&desc, &self.samplers, *s, &self.refl[*h].bindings)) {
                    return Err("left out: root signature doesn't cover a binding");
                }
                let samplers = &self.samplers;
                Ok(self.rs_of.entry(desc).or_insert_with_key(|d| keep(d3d12::serialize(d, samplers))).clone())
            }
            RootSigs::Embedded => {
                // a pipeline's stages share one root signature: each must carry the same one
                let parts: Vec<Option<&[u8]>> = stages.iter().map(|(_, h)| crate::dxbc::part(&self.set[*h].container, b"RTS0")).collect();
                let Some(Some(first)) = parts.first().copied() else { return Err("left out: a stage carries no root signature") };
                if parts.iter().any(|p| *p != Some(first)) {
                    return Err("left out: stages carry different root signatures");
                }
                let part = psodb::hex(&<sha1::Sha1 as sha1::Digest>::digest(first));
                let container = &self.set[stages[0].1].container;
                self.embedded_of
                    .entry(part)
                    .or_insert_with(|| d3d12::as_version10(container).map(&mut keep))
                    .clone()
                    .ok_or("left out: root signature has no 1.0 form")
            }
        }
    }

    fn emit(&mut self, mut stages: Vec<(u32, &'a str)>) {
        stages.sort();
        if !self.seen.insert(stages.clone()) {
            return; // shaders shared across maps
        }
        let vs = stages.iter().find(|(s, _)| *s == VS).map(|(_, h)| &self.refl[*h]);
        let rs = match self.root_sig(&stages, vs) {
            Ok(rs) => rs,
            Err(why) => {
                *self.stats.entry(why).or_default() += 1;
                return;
            }
        };
        self.used.extend(stages.iter().map(|(_, h)| *h));
        if stages[0].0 == CS {
            self.items.push(Rec::new(b'C', d3d12::compute_record(&rs, stages[0].1)));
            *self.stats.entry("compute").or_default() += 1;
            return;
        }
        let layout = vs.map(d3d12::vs_layout).unwrap_or_default();
        let (rt, dsv) = stages.iter().find(|(s, _)| *s == PS).map(|(_, h)| d3d12::targets(&self.refl[*h])).unwrap_or_default();
        let topology = match stages.iter().find(|(s, _)| *s == GS) {
            Some((_, g)) => d3d12::topology_type(self.refl[*g].gs_input),
            None => 3, // triangles
        };
        if topology == 0 {
            *self.stats.entry("left out: GS input topology unknown").or_default() += 1;
            return;
        }
        let shape = format!("{:?}|{topology}|{rt:?}|{dsv}", stages.iter().map(|(s, _)| s).collect::<Vec<_>>());
        let label = match stages.iter().map(|(s, _)| *s).collect::<Vec<_>>()[..] {
            [VS] => "VS alone",
            [VS, PS] => "VS+PS",
            [VS, GS] => "VS+GS",
            _ => "VS+GS+PS",
        };
        *self.stats.entry(label).or_default() += 1;
        match self.templates.get(&shape) {
            Some(t) => self.items.push(Rec::new(b'P', d3d12::item(t, &rs, &stages, &layout))),
            None => {
                // the first pipeline of a shape is its template, compiled as itself
                let r = Rec::new(b'S', d3d12::stream(&rs, &stages, &layout, topology, &rt, dsv));
                self.templates.insert(shape, r.key());
                self.streams.push(r);
            }
        }
    }
}

/// A map's shaders count as shared when they're in this share of its maps or more (PlanBuilder.SharedMaps).
const SHARED_MAPS: usize = 50;

/// `src` feeds `dst` (d3d12::links), with the DXBC register rule when `dst` is DXBC.
fn links(set: &HashMap<String, Shader>, refl: &HashMap<&str, Reflection>, src: &str, dst: &str) -> bool {
    d3d12::links(&refl[src], &refl[dst], crate::dxbc::part(&set[dst].container, b"DXIL").is_none())
}

/// `gpu_lanes`: the wave sizes the GPU runs; a shader requiring a size outside them is left out (the runtime rejects it, and
/// Unreal only picks the permutations the GPU runs).
pub fn write(set: &HashMap<String, Shader>, maps: &[ShaderMap], rs_mode: RootSigs, ue5: bool, gpu_lanes: (u32, u32), out: &Path, what: String) -> Result<PlanOutcome, String> {
    let runs = |s: &Shader| d3d12::wave_lanes(&s.container).map(|(lo, hi)| lo <= gpu_lanes.1 && hi >= gpu_lanes.0).unwrap_or(true);
    let wave_out = set.values().filter(|s| s.stage != 0 && !runs(s)).count();
    let refl: HashMap<&str, Reflection> = set.iter().filter(|(_, s)| s.stage != 0 && runs(s)).map(|(h, s)| (h.as_str(), d3d12::reflect(&s.container))).collect();
    let mut b = Builder {
        set,
        refl: &refl,
        max_srv: match rs_mode {
            RootSigs::Unreal { rule, .. } => d3d12::max_srvs_for(rule, set.values().map(|s| s.counts)),
            RootSigs::Embedded => 0,
        },
        samplers: match rs_mode {
            RootSigs::Unreal { rule, .. } => d3d12::ue_samplers(rule),
            RootSigs::Embedded => Vec::new(),
        },
        rs_mode: &rs_mode,
        rs_of: HashMap::new(),
        embedded_of: HashMap::new(),
        rs_blobs: Vec::new(),
        templates: HashMap::new(),
        seen: HashSet::new(),
        used: HashSet::new(),
        streams: Vec::new(),
        items: Vec::new(),
        stats: BTreeMap::new(),
    };
    let mut css: Vec<&str> = set.iter().filter(|(h, s)| s.stage == CS && refl.contains_key(h.as_str())).map(|(h, _)| h.as_str()).collect();
    css.sort();
    for cs in css {
        b.emit(vec![(CS, cs)]);
    }
    // the map's shaders as keys of `set` (what the builder holds on to), with their reflection
    let shas = |m: &ShaderMap| -> Vec<&str> { m.shas.iter().filter_map(|h| set.get_key_value(h)).map(|(k, _)| k.as_str()).filter(|h| refl.contains_key(h)).collect() };
    let stage = |h: &str| set[h].stage;
    let mut fed: HashSet<&str> = HashSet::new();
    let mut unfed: Vec<&str> = Vec::new();
    for map in maps {
        let ds = shas(map);
        let mut srcs: HashMap<String, Vec<&str>> = HashMap::new();
        let mut gss: HashMap<String, Vec<&str>> = HashMap::new();
        for &h in &ds {
            match stage(h) {
                VS => {
                    srcs.entry(d3d12::sig_key(&refl[h].outputs, true)).or_default().push(h);
                    if d3d12::rasterizable(&refl[h]) {
                        b.emit(vec![(VS, h)]); // depth passes draw a VS without a PS
                    }
                }
                GS => gss.entry(d3d12::sig_key(&refl[h].outputs, true)).or_default().push(h),
                _ => {}
            }
        }
        for &p in ds.iter().filter(|h| stage(h) == PS) {
            let pin = d3d12::sig_key(&refl[p].inputs, true);
            let mut sets: Vec<Vec<(u32, &str)>> = Vec::new();
            for &v in srcs.get(&pin).into_iter().flatten() {
                if d3d12::rasterizable(&refl[v]) && links(set, &refl, v, p) {
                    sets.push(vec![(VS, v), (PS, p)]);
                }
            }
            for &g in gss.get(&pin).into_iter().flatten().filter(|g| links(set, &refl, g, p)) {
                for &v in srcs.get(&d3d12::sig_key(&refl[g].inputs, true)).into_iter().flatten().filter(|v| links(set, &refl, v, g)) {
                    sets.push(vec![(VS, v), (GS, g), (PS, p)]);
                }
            }
            if !sets.is_empty() {
                fed.insert(p);
            } else if !map.pooled {
                unfed.push(p);
            }
            sets.into_iter().for_each(|st| b.emit(st));
        }
        // VS -> GS without a PS: UE's one-pass point-light shadows draw casters into a cube map's depth through a GS
        for &g in ds.iter().filter(|h| stage(h) == GS && d3d12::rasterizable(&refl[**h])) {
            for &v in srcs.get(&d3d12::sig_key(&refl[g].inputs, true)).into_iter().flatten().filter(|v| links(set, &refl, v, g)) {
                b.emit(vec![(VS, v), (GS, g)]);
            }
        }
    }
    // global VS -> GS chains also feed material PSs (rendering into volume textures): paired across libraries
    let gpool: Vec<&str> = maps.iter().filter(|m| m.pooled).flat_map(|m| shas(m)).collect();
    let mut ps_by_in: HashMap<String, Vec<&str>> = HashMap::new();
    for m in maps.iter().filter(|m| !m.pooled) {
        for p in shas(m).into_iter().filter(|h| stage(h) == PS) {
            ps_by_in.entry(d3d12::sig_key(&refl[p].inputs, true)).or_default().push(p);
        }
    }
    for &g in gpool.iter().filter(|h| stage(h) == GS) {
        let gin = d3d12::sig_key(&refl[g].inputs, true);
        let vss: Vec<&str> = gpool.iter().copied().filter(|v| stage(v) == VS && d3d12::sig_key(&refl[v].outputs, true) == gin && links(set, &refl, v, g)).collect();
        let pss: Vec<&str> = ps_by_in.get(&d3d12::sig_key(&refl[g].outputs, true)).into_iter().flatten().copied().filter(|p| links(set, &refl, g, p)).collect();
        for &p in &pss {
            for &v in &vss {
                b.emit(vec![(VS, v), (GS, g), (PS, p)]);
            }
        }
    }
    // a material PS nothing in its own map feeds is drawn behind a global VS (UE 5.1 Nanite material passes)
    let mut gvs: HashMap<String, Vec<&str>> = HashMap::new();
    for &v in gpool.iter().filter(|h| stage(h) == VS && d3d12::rasterizable(&refl[**h])) {
        gvs.entry(d3d12::sig_key(&refl[v].outputs, true)).or_default().push(v);
    }
    let mut seen_orphan = HashSet::new();
    for &p in unfed.iter().filter(|p| !fed.contains(**p)) {
        if seen_orphan.insert(p) {
            for &v in gvs.get(&d3d12::sig_key(&refl[p].inputs, true)).into_iter().flatten().filter(|v| links(set, &refl, v, p)) {
                b.emit(vec![(VS, v), (PS, p)]);
            }
        }
    }
    // UE5: VS -> PS across maps where the PS is shared by many maps (a default material's), SharedAcrossMaps
    if ue5 {
        let own: Vec<&ShaderMap> = maps.iter().filter(|m| !m.pooled).collect();
        let mut in_maps: HashMap<&str, usize> = HashMap::new();
        for m in &own {
            let distinct: HashSet<&str> = shas(m).into_iter().collect();
            distinct.into_iter().for_each(|h| *in_maps.entry(h).or_default() += 1);
        }
        let shared = |h: &str| in_maps.get(h).map(|&n| n > 1 && n * SHARED_MAPS >= own.len()).unwrap_or(false);
        let mut shared_in: HashMap<String, Vec<&str>> = HashMap::new();
        for &p in in_maps.keys().filter(|h| stage(h) == PS && shared(h)) {
            shared_in.entry(d3d12::sig_key(&refl[p].inputs, true)).or_default().push(p);
        }
        let mut vss: Vec<&str> = in_maps.keys().copied().filter(|h| stage(h) == VS && d3d12::rasterizable(&refl[*h])).collect();
        vss.sort();
        for v in vss {
            let mut pss: Vec<&str> = shared_in.get(&d3d12::sig_key(&refl[v].outputs, true)).into_iter().flatten().copied().filter(|p| links(set, &refl, v, p)).collect();
            pss.sort();
            for p in pss {
                b.emit(vec![(VS, v), (PS, p)]);
            }
        }
    }
    let unpaired = maps.iter().flat_map(|m| shas(m)).filter(|h| stage(h) == PS && !b.used.contains(h)).collect::<HashSet<_>>().len();
    if unpaired > 0 {
        b.stats.insert("PS left unpaired", unpaired);
    }
    if wave_out > 0 {
        b.stats.insert("left out: a wave size this GPU doesn't run", wave_out);
    }
    let planned = b.streams.len() + b.items.len();
    if planned == 0 {
        return Err(format!("{what}: no pipelines to plan"));
    }
    let mut used: Vec<&str> = b.used.iter().copied().collect();
    used.sort();
    let res = (|| -> std::io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(out)?);
        for (h, blob) in &b.rs_blobs {
            psodb::write_blob(&mut w, h, blob)?;
        }
        for h in used {
            psodb::write_blob(&mut w, h, &set[h].container)?;
        }
        // templates before the items that name them: the proxy resolves items as it loads
        for r in b.streams.iter().chain(&b.items) {
            psodb::write(&mut w, r.tag, &r.payload)?;
        }
        w.flush()
    })();
    res.map_err(|e| format!("couldn't write the plan: {e}"))?;
    let stats: Vec<String> = b.stats.iter().map(|(k, v)| format!("{k} {v}")).collect();
    Ok(PlanOutcome {
        planned_items: planned,
        partial: true,
        label: String::new(),
        summary: format!("{what} -> {planned} pipelines over {} root signatures ({})", b.rs_blobs.len(), stats.join(", ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::d3d12::test_shader as shader;

    #[test]
    fn pairs_linking_shaders_of_a_map() {
        let pos = ("SV_POSITION", 0, 0, 0xF, 1);
        let vs = shader(1, &[("POSITION", 0, 0, 0x7, 0)], &[pos, ("TEXCOORD", 0, 1, 0x3, 0)]);
        let ps = shader(0, &[pos, ("TEXCOORD", 0, 1, 0x3, 0)], &[("SV_TARGET", 0, 0, 0xF, 64)]);
        let ps_other = shader(0, &[pos, ("COLOR", 0, 1, 0xF, 0)], &[("SV_TARGET", 0, 0, 0xF, 64)]);
        let counts = d3d12::Counts { cb: 1, srv: 1, ..Default::default() };
        let mut set = HashMap::new();
        let mut map = Vec::new();
        for (c, stage) in [(vs, VS), (ps, PS), (ps_other, PS)] {
            let h = psodb::hex(&<sha1::Sha1 as sha1::Digest>::digest(&c));
            map.push(h.clone());
            set.insert(h, Shader { stage, stage11: 0, counts, container: c });
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("gen.db");
        let o = write(&set, &[ShaderMap { pooled: false, shas: map }], RootSigs::Unreal { rule: Rule::Ue55, mesh_tier: false }, true, (32, 32), &out, "test".into()).unwrap();
        assert_eq!(o.planned_items, 2, "{}", o.summary); // VS alone + VS->PS; the COLOR PS isn't fed
        let recs = psodb::read_all(&out).unwrap();
        let tags: Vec<char> = recs.iter().map(|r| r.tag as char).collect();
        assert_eq!(tags.iter().filter(|t| **t == 'S').count(), 2); // two shapes: no PS, one RGBA16F target
        // every hash a record names is a blob of the plan
        let blobs: HashSet<String> = recs.iter().filter_map(|r| (r.tag == b'B').then(|| r.blob_hash().unwrap())).collect();
        for r in recs.iter().filter(|r| r.tag == b'S') {
            let p = psodb::parse(r).unwrap();
            assert!(blobs.contains(&p.rs) && p.stages.values().all(|h| blobs.contains(h)));
        }
    }

    #[test]
    fn ue5_pairs_a_vs_with_ps_shared_by_other_maps() {
        let pos = ("SV_POSITION", 0, 0, 0xF, 1);
        let tex = ("TEXCOORD", 0, 1, 0x3, 0);
        let rt = ("SV_TARGET", 0, 0, 0xF, 64);
        let vs = shader(1, &[("POSITION", 0, 0, 0x7, 0)], &[pos, tex]);
        let ps = shader(0, &[pos, tex], &[rt]);
        let ps_color = shader(0, &[pos, ("COLOR", 0, 1, 0xF, 0)], &[rt]);
        let mut set = HashMap::new();
        let mut h = Vec::new();
        for (c, stage) in [(vs, VS), (ps, PS), (ps_color, PS)] {
            let k = psodb::hex(&<sha1::Sha1 as sha1::Digest>::digest(&c));
            h.push(k.clone());
            set.insert(k, Shader { stage, stage11: 0, counts: Default::default(), container: c });
        }
        let map = |v: &[&String]| ShaderMap { pooled: false, shas: v.iter().map(|x| x.to_string()).collect() };
        // the VS's map has no PS; the TEXCOORD PS is in two of the three maps
        let maps = [map(&[&h[0]]), map(&[&h[1], &h[2]]), map(&[&h[1]])];
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("gen.db");
        let o = write(&set, &maps, RootSigs::Unreal { rule: Rule::Ue55, mesh_tier: false }, true, (32, 64), &out, "test".into()).unwrap();
        assert_eq!(o.planned_items, 2, "{}", o.summary); // VS alone, VS + shared PS
        assert!(o.summary.contains("PS left unpaired 1"), "{}", o.summary);
        let o4 = write(&set, &maps, RootSigs::Unreal { rule: Rule::Ue426, mesh_tier: false }, false, (32, 64), &out, "test".into()).unwrap();
        assert_eq!(o4.planned_items, 1, "{}", o4.summary); // UE4: no cross-map pairing
    }

    #[test]
    fn embedded_root_signatures_pair_only_matching_stages() {
        use crate::engines::d3d12::test_shader_with;
        let pos = ("SV_POSITION", 0, 0, 0xF, 1);
        let tex = ("TEXCOORD", 0, 1, 0x3, 0);
        let rt = ("SV_TARGET", 0, 0, 0xF, 64);
        // two 1.1 root signatures: no parameters, flags 1 (input layout) or 0
        let rs = |flags: u32| [2u32, 0, 24, 0, 24, flags].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let (a, b) = (rs(1), rs(0));
        let shaders = [
            (test_shader_with(1, &[("POSITION", 0, 0, 0x7, 0)], &[pos, tex], Some(&a)), VS),
            (test_shader_with(0, &[pos, tex], &[rt], Some(&a)), PS),
            (test_shader_with(0, &[pos, tex], &[("SV_TARGET", 1, 1, 0xF, 64)], Some(&b)), PS), // another root signature
            (test_shader_with(5, &[], &[], None), CS),                                       // none
        ];
        let mut set = HashMap::new();
        let mut map = Vec::new();
        for (c, stage) in shaders {
            let h = psodb::hex(&<sha1::Sha1 as sha1::Digest>::digest(&c));
            map.push(h.clone());
            set.insert(h, Shader { stage, stage11: 0, counts: Default::default(), container: c });
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("gen.db");
        let o = write(&set, &[ShaderMap { pooled: false, shas: map }], RootSigs::Embedded, false, (32, 64), &out, "test".into()).unwrap();
        assert_eq!(o.planned_items, 2, "{}", o.summary); // VS alone, VS + the PS of its root signature
        assert!(o.summary.contains("stages carry different root signatures 1") && o.summary.contains("carries no root signature 1"), "{}", o.summary);
        let recs = psodb::read_all(&out).unwrap();
        let rs_blobs: Vec<&psodb::Rec> = recs.iter().filter(|r| r.tag == b'B' && crate::dxbc::part(&r.payload[20..], b"SHEX").is_none()).collect(); // root signatures: no program
        assert_eq!(rs_blobs.len(), 1);
        assert_eq!(u32::from_le_bytes(crate::dxbc::part(&rs_blobs[0].payload[20..], b"RTS0").unwrap()[..4].try_into().unwrap()), 1); // at version 1.0
    }
}

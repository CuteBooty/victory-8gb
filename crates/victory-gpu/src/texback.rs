//! Shader-written textures the title samples on the CPU. A list copies each
//! selected texture once, then tiles its read-back after the GPU has finished.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use victory_formats::texture::Layout;
use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;
use crate::resource::{self, TextureRef};

type Kind = (u64, u32, u32);
// The four kinds identified by the upstream read watch.
const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";

fn parse(list: &str) -> Vec<Kind> {
    list.split(',')
        .filter_map(|k| {
            let (size, format) = k.trim().split_once('f')?;
            let (width, height) = size.split_once('x')?;
            Some((width.parse().ok()?, height.parse().ok()?, format.parse().ok()?))
        })
        .collect()
}

fn kinds() -> &'static [Kind] {
    static KINDS: OnceLock<Vec<Kind>> = OnceLock::new();
    KINDS.get_or_init(|| {
        let kinds = parse(&std::env::var("VICTORY_TEXTURE_BACK").unwrap_or_else(|_| DEFAULT.into()));
        if !kinds.is_empty() {
            tracing::info!(?kinds, "shader-written textures selected for copy-back (VICTORY_TEXTURE_BACK)");
        }
        kinds
    })
}

pub(crate) fn enabled() -> bool {
    !kinds().is_empty()
}

pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty()
        && resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
        && resource::dropped(resource) == 0
}

pub(crate) struct Back {
    resource: TextureRef,
    kind: Kind,
    at: usize,
    layout: Layout,
    buffer: ID3D12Resource,
    bytes: usize,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
}

impl Back {
    pub(crate) fn new(resource: TextureRef, texture: &ID3D12Resource) -> Option<Self> {
        let kind = resource::texture_kind(resource.get())?;
        let (at, format, layout) = resource.destination()?;
        // SAFETY: the list holds the live host texture.
        let d = unsafe { texture.GetDesc() };
        if !layout.addressable()
            || layout.desc.samples != 1
            || d.SampleDesc.Count != 1
            || resource::family(d.Format.0 as u32) != resource::family(format)
            || d.Width != u64::from(layout.desc.width)
            || d.Height != layout.desc.height
            || u32::from(d.MipLevels) != layout.desc.mips
            || u32::from(d.DepthOrArraySize) != layout.desc.depth
        {
            return None;
        }
        let size = usize::try_from(layout.size).ok()?;
        if at == 0 || size == 0 || size > isize::MAX as usize {
            return None;
        }
        at.checked_add(size)?;
        let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            layout.desc.mips
        } else {
            layout.desc.mips.checked_mul(layout.desc.depth)?
        };
        let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
        let mut rows = vec![0; count as usize];
        let mut total = 0;
        let host = crate::host::host()?;
        // SAFETY: both out arrays have one entry per subresource.
        unsafe {
            host.device.GetCopyableFootprints(
                &d,
                0,
                count,
                0,
                Some(footprints.as_mut_ptr()),
                Some(rows.as_mut_ptr()),
                None,
                Some(&mut total),
            );
        }
        if total == 0 || total > isize::MAX as u64 {
            return None;
        }
        let buffer = host.read_back_buffer(total)?;
        Some(Self { resource, kind, at, layout, buffer, bytes: total as usize, footprints, rows })
    }

    pub(crate) fn span(&self) -> (usize, usize) {
        (self.at, self.at + self.layout.size as usize)
    }

    pub(crate) fn record(&self, list: &ID3D12GraphicsCommandList, texture: &ID3D12Resource) {
        for (i, footprint) in self.footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: the location borrows the buffer.
                pResource: unsafe { std::mem::transmute_copy(&self.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: the location borrows the texture.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
            };
            // SAFETY: an open list, the texture in COPY_SOURCE, the buffer in COPY_DEST.
            unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }

    fn copy_to_title(&self) -> Result<(), Skip> {
        let size = self.layout.size as usize;
        let mut tiled = Vec::new();
        if tiled.try_reserve_exact(size).is_err() {
            return Err(Skip::Other);
        }
        tiled.resize(size, 0);
        // No change of mapping there until the write is done.
        let _mapping = crate::watch::title_memory_reading(self.at, size);
        // Memory the title gave up, or that shows other pages now, is not
        // what the shader wrote for.
        if !self.resource.title_holds() {
            return Err(Skip::Released);
        }
        if crate::watch::remapped_since(self.at, size, self.resource.epoch()) {
            return Err(Skip::Remapped);
        }
        // Preserve padding and skip a texture with any missing page.
        if !crate::host::read_into(self.at as *const u8, &mut tiled) {
            return Err(Skip::Other);
        }
        let mut data = std::ptr::null_mut();
        // SAFETY: the GPU completed the list that filled this buffer.
        if unsafe { self.buffer.Map(0, None, Some(&mut data)) }.is_err() {
            return Err(Skip::Other);
        }
        let ready = !data.is_null() && {
            // SAFETY: the mapping spans exactly the buffer size passed at creation.
            let source = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), self.bytes) };
            tile(&self.layout, &self.footprints, &self.rows, &mut tiled, source).is_some()
        };
        // SAFETY: no CPU writes to the mapped buffer.
        unsafe { self.buffer.Unmap(0, Some(&D3D12_RANGE::default())) };
        if !ready {
            return Err(Skip::Other);
        }
        if crate::host::write_texture(self.at, &tiled) { Ok(()) } else { Err(Skip::Short) }
    }

    pub(crate) fn land(&self) {
        let start = Instant::now();
        let result = self.copy_to_title();
        let elapsed = start.elapsed();
        let mut stats = STATS.lock().unwrap_or_else(|e| e.into_inner());
        stats.add(self.kind, self.layout.size, result);
        stats.time += elapsed;
    }
}

/// Why a texture did not land.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Skip {
    /// Its bytes were mapped anew since the shader wrote it.
    Remapped,
    /// The title released it.
    Released,
    /// Windows took only part of the write.
    Short,
    /// A missing page, no memory, a failed map or tiling.
    Other,
}

fn tile(
    layout: &Layout,
    footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
    rows: &[u32],
    tiled: &mut [u8],
    linear: &[u8],
) -> Option<()> {
    for (i, footprint) in footprints.iter().enumerate() {
        // The last row need not include the trailing pitch padding.
        let end = footprints.get(i + 1).map_or(linear.len(), |next| next.Offset as usize);
        let source = linear.get(footprint.Offset as usize..end)?;
        let pitch = footprint.Footprint.RowPitch as usize;
        let depth_pitch = pitch.checked_mul(rows[i] as usize)?;
        let mips = layout.desc.mips;
        layout.tile(tiled, i as u32 % mips, i as u32 / mips, source, pitch, depth_pitch).ok()?;
    }
    Some(())
}

/// Since the last report: textures landed and their bytes by kind, the
/// time spent, and skips by reason (in [`Skip`]'s order).
#[derive(Default)]
struct Stats {
    kinds: Vec<(Kind, u64, u64)>,
    time: Duration,
    skipped: [u64; 4],
}

impl Stats {
    const fn new() -> Self {
        Stats { kinds: Vec::new(), time: Duration::ZERO, skipped: [0; 4] }
    }

    fn add(&mut self, kind: Kind, bytes: u64, result: Result<(), Skip>) {
        match result {
            Ok(()) => match self.kinds.iter_mut().find(|k| k.0 == kind) {
                Some(k) => {
                    k.1 += 1;
                    k.2 += bytes;
                }
                None => self.kinds.push((kind, 1, bytes)),
            },
            Err(skip) => self.skipped[skip as usize] += 1,
        }
    }

    fn totals(&self) -> (u64, u64) {
        self.kinds.iter().fold((0, 0), |(n, b), k| (n + k.1, b + k.2))
    }

    /// `64x64f16: 120 textures 3.8 MB; ...; skipped: remapped 0, ...`
    fn line(&self) -> String {
        let mut line = String::new();
        for &((w, h, f), n, bytes) in &self.kinds {
            line += &format!("{w}x{h}f{f}: {n} textures {:.1} MB; ", bytes as f64 / 1e6);
        }
        let [remapped, released, short, other] = self.skipped;
        line + &format!("skipped: remapped {remapped}, released {released}, short {short}, other {other}")
    }
}

static STATS: Mutex<Stats> = Mutex::new(Stats::new());

/// Since the last report: texture copy-backs that landed only after a
/// present had returned to the title since their list was submitted (the
/// title may have read them before; `crate::host::settle_textures` is to
/// make this 0).
static AFTER_PRESENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn landed_after_present(n: u64) {
    AFTER_PRESENT.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn log_interval() {
    if enabled() {
        let stats = std::mem::take(&mut *STATS.lock().unwrap_or_else(|e| e.into_inner()));
        let (textures, bytes) = stats.totals();
        let after_present = AFTER_PRESENT.swap(0, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            textures,
            bytes,
            after_present,
            cpu_ms = stats.time.as_secs_f64() * 1000.0,
            kinds = %stats.line(),
            "shader texture copy-back since last report"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_back_selectors() {
        assert_eq!(parse(DEFAULT), [(256, 256, 41), (256, 256, 16), (64, 64, 16), (128, 128, 36)]);
        assert!(parse("0").is_empty());
        assert_eq!(parse("32x8f41,invalid, 16x4f16 "), [(32, 8, 41), (16, 4, 16)]);
    }

    #[test]
    fn padded_rows_land_in_each_title_mip_and_array_slice() {
        use victory_formats::texture::{Desc, Dimension, Swizzle};
        let layout = Layout::new(&Desc {
            dimension: Dimension::Tex2d,
            width: 3,
            height: 2,
            depth: 2,
            mips: 2,
            element_bytes: 1,
            block: 1,
            samples: 1,
            swizzle: Swizzle::LINEAR,
        })
        .unwrap();
        let footprints = [0, 512, 1024, 1536].map(|offset| D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: offset,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT { RowPitch: 256, ..Default::default() },
        });
        let rows = [2, 1, 2, 1];
        let mut linear = vec![0; 1537];
        let mut expected = vec![0xa5; layout.size as usize];
        for (i, fp) in footprints.iter().enumerate() {
            let (mip, slice) = (i as u32 % 2, i as u32 / 2);
            let m = &layout.mips[mip as usize];
            for y in 0..m.height {
                for x in 0..m.width {
                    let value = (i as u32 * 16 + 4 * y + x) as u8;
                    linear[fp.Offset as usize + y as usize * 256 + x as usize] = value;
                    expected[layout.offset(mip, x, y, slice).unwrap() as usize] = value;
                }
            }
        }
        let mut tiled = vec![0xa5; expected.len()];
        assert_eq!(tile(&layout, &footprints, &rows, &mut tiled, &linear), Some(()));
        assert_eq!(tiled, expected);
        assert!(tile(&layout, &footprints, &rows, &mut tiled, &linear[..1536]).is_none());
    }

    #[test]
    fn statistics_by_kind_and_skip() {
        let mut stats = Stats::new();
        let (small, big) = ((64, 64, 16), (256, 256, 41));
        for _ in 0..3 {
            stats.add(small, 32_768, Ok(()));
        }
        stats.add(big, 1_000_000, Ok(()));
        stats.add(small, 32_768, Err(Skip::Remapped));
        stats.add(big, 1_000_000, Err(Skip::Released));
        stats.add(big, 1_000_000, Err(Skip::Released));
        stats.add(big, 1_000_000, Err(Skip::Short));
        assert_eq!(stats.totals(), (4, 3 * 32_768 + 1_000_000));
        assert_eq!(
            stats.line(),
            "64x64f16: 3 textures 0.1 MB; 256x256f41: 1 textures 1.0 MB; \
             skipped: remapped 1, released 2, short 1, other 0"
        );
        let taken = std::mem::take(&mut stats);
        assert_eq!(taken.totals().0, 4);
        assert_eq!(stats.line(), "skipped: remapped 0, released 0, short 0, other 0");
    }
}

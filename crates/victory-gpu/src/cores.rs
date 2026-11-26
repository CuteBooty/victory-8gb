//! SPDX-License-Identifier: GPL-3.0-or-later
//!
//! Cache enumeration and worker placement adapted from Jake (avatarmanz/VIctory-7900XTX 7c3d839c), GPL-3.0-or-later.
//! VICTORY_CORE_PLAN selects two comparable last-level caches in processor order.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Plan {
    OursA,
    OursB,
    Split,
    TitleA,
    Together,
}

impl Plan {
    fn parse(value: Option<&str>) -> Option<Self> {
        Some(match value? {
            "ours-a" => Self::OursA,
            "ours-b" => Self::OursB,
            "split" => Self::Split,
            "title-a" => Self::TitleA,
            "together" => Self::Together,
            _ => return None,
        })
    }

    /// Index in the selected pair. Only the two title plans touch title threads.
    fn target(self, role: Role) -> Option<usize> {
        match (self, role) {
            (Self::TitleA | Self::Together, Role::Title) => Some(0),
            (_, Role::Title) => None,
            (Self::OursB | Self::TitleA, _) | (Self::Split, Role::Helper) => Some(1),
            _ => Some(0),
        }
    }
}

#[derive(Clone, Copy)]
enum Role {
    Title,
    SubmitOrRelay,
    Helper,
}

#[derive(Clone, Default)]
struct Cache {
    bytes: u32,
    masks: Vec<(u16, usize)>, // Windows processor group and logical processors
    #[cfg(windows)]
    ids: Vec<u32>,
    classes: Vec<u8>, // one efficiency class per logical processor, sorted
}

impl std::fmt::Display for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, (group, mask)) in self.masks.iter().enumerate() {
            if i != 0 {
                write!(f, "+")?;
            }
            write!(f, "{group}/{mask:#x}")?;
        }
        Ok(())
    }
}

fn pair(caches: &[Cache], hard: bool) -> Option<[&Cache; 2]> {
    let eligible = |c: &&Cache| {
        let width: usize = c.masks.iter().map(|(_, m)| m.count_ones() as usize).sum();
        c.bytes > 0 && width > 0 && c.classes.len() == width && (!hard || c.masks.len() == 1)
    };
    let mut sorted: Vec<_> = caches.iter().filter(eligible).collect();
    sorted.sort_by_key(|c| (c.masks[0].0, c.masks[0].1.trailing_zeros()));
    for (i, &a) in sorted.iter().enumerate() {
        for &b in &sorted[i + 1..] {
            if a.bytes == b.bytes
                && a.classes == b.classes
                && !a
                    .masks
                    .iter()
                    .any(|(g, m)| b.masks.iter().any(|(h, n)| g == h && m & n != 0))
            {
                return Some([a, b]);
            }
        }
    }
    None
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::io;
    use std::mem::offset_of;
    use std::os::windows::io::{AsRawHandle, BorrowedHandle};
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::SystemInformation::*;
    use windows_sys::Win32::System::Threading::*;

    fn invalid() -> io::Error {
        io::Error::from(io::ErrorKind::InvalidData)
    }

    /// Both APIs return variable-length records. Their Size fields are at different offsets.
    fn records(
        size_at: usize,
        query: impl Fn(*mut u8, u32, &mut u32) -> i32,
        mut visit: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut words = Vec::<u64>::new();
        let mut capacity = 0;
        let used = loop {
            let mut bytes = capacity;
            let ptr = if capacity == 0 {
                std::ptr::null_mut()
            } else {
                words.as_mut_ptr().cast()
            };
            if query(ptr, capacity, &mut bytes) != 0 {
                if bytes > capacity {
                    return Err(invalid());
                }
                break bytes as usize;
            }
            if bytes <= capacity {
                return Err(io::Error::last_os_error());
            }
            words.resize((bytes as usize).div_ceil(8), 0);
            capacity = bytes;
        };
        // SAFETY: the API filled used bytes in this aligned, initialized buffer.
        let mut data = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), used) };
        while !data.is_empty() {
            let field = data.get(size_at..size_at + 4).ok_or_else(invalid)?;
            let size = u32::from_ne_bytes(field.try_into().unwrap()) as usize;
            if size < size_at + 4 || size > data.len() {
                return Err(invalid());
            }
            visit(&data[..size])?;
            data = &data[size..];
        }
        Ok(())
    }

    /// Only used for the integer-only Win32 records below (all bit patterns valid).
    unsafe fn field<T: Copy>(record: &[u8], at: usize) -> io::Result<T> {
        let data = record
            .get(at..)
            .filter(|v| v.len() >= size_of::<T>())
            .ok_or_else(invalid)?;
        // SAFETY: checked size; the caller supplies the type of this field.
        Ok(unsafe { data.as_ptr().cast::<T>().read_unaligned() })
    }

    fn topology() -> io::Result<Vec<Cache>> {
        type Info = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
        let mut caches = Vec::new();
        let mut level = 0;
        records(
            offset_of!(Info, Size),
            // SAFETY: size query or aligned writable buffer of the size in n.
            |p, _, n| unsafe { GetLogicalProcessorInformationEx(RelationCache, p.cast(), n) },
            |r| {
                let at = offset_of!(Info, Anonymous);
                // SAFETY: requested only RelationCache; read its member, not the larger union.
                let c: CACHE_RELATIONSHIP = unsafe { field(r, at)? };
                if (c.Type != CacheUnified && c.Type != CacheData) || c.Level < level {
                    return Ok(());
                }
                if c.Level > level {
                    caches.clear();
                    level = c.Level;
                }
                let mut masks = Vec::new();
                for i in 0..usize::from(c.GroupCount.max(1)) {
                    let pos = at
                        + offset_of!(CACHE_RELATIONSHIP, Anonymous)
                        + i * size_of::<GROUP_AFFINITY>();
                    // SAFETY: bounds-checked entry of the trailing GroupMasks array.
                    let m: GROUP_AFFINITY = unsafe { field(r, pos)? };
                    if m.Mask == 0 {
                        return Err(invalid());
                    }
                    masks.push((m.Group, m.Mask));
                }
                masks.sort_unstable();
                caches.push(Cache {
                    bytes: c.CacheSize,
                    masks,
                    ..Cache::default()
                });
                Ok(())
            },
        )?;
        records(
            offset_of!(SYSTEM_CPU_SET_INFORMATION, Size),
            // SAFETY: size query or aligned writable buffer for our own process.
            |p, cap, n| unsafe {
                GetSystemCpuSetInformation(p.cast(), cap, n, GetCurrentProcess(), 0)
            },
            |r| {
                // SAFETY: fixed Type field; the CpuSet member is read only for records of that type.
                let kind: i32 = unsafe { field(r, offset_of!(SYSTEM_CPU_SET_INFORMATION, Type))? };
                if kind == CpuSetInformation {
                    let info: SYSTEM_CPU_SET_INFORMATION = unsafe { field(r, 0)? };
                    let s = unsafe { info.Anonymous.CpuSet };
                    let bit = 1usize
                        .checked_shl(s.LogicalProcessorIndex.into())
                        .ok_or_else(invalid)?;
                    for c in &mut caches {
                        if c.masks.iter().any(|&(g, m)| g == s.Group && m & bit != 0) {
                            c.ids.push(s.Id);
                            c.classes.push(s.EfficiencyClass);
                        }
                    }
                }
                Ok(())
            },
        )?;
        for c in &mut caches {
            c.classes.sort_unstable();
        }
        Ok(caches)
    }

    struct Placement {
        plan: Plan,
        hard: bool,
        caches: [Cache; 2],
    }
    static PLACED: AtomicU64 = AtomicU64::new(0);
    static ACTIVE: LazyLock<Option<Placement>> = LazyLock::new(|| {
        let value = std::env::var_os("VICTORY_CORE_PLAN")?;
        let Some(plan) = Plan::parse(value.to_str()) else {
            tracing::warn!(?value, "unknown core plan; placement disabled");
            return None;
        };
        let hard = std::env::var_os("VICTORY_CORE_HARD").is_some_and(|s| s == "1");
        let caches = topology()
            .inspect_err(
                |e| tracing::warn!(plan = ?value, %e, "cache discovery failed; placement disabled"),
            )
            .ok()?;
        let found: Vec<_> = caches
            .iter()
            .map(|c| format!("{} B on {c}", c.bytes))
            .collect();
        let Some([a, b]) = pair(&caches, hard) else {
            tracing::info!(
                plan = ?value,
                hard,
                ?found,
                "no comparable last-level cache pair; placement disabled"
            );
            return None;
        };
        tracing::info!(plan = ?value, hard, ?found, a = %a, b = %b, "last-level cache placement");
        Some(Placement {
            plan,
            hard,
            caches: [a.clone(), b.clone()],
        })
    });

    pub fn enabled() -> bool {
        ACTIVE.is_some()
    }

    pub fn places_titles() -> bool {
        ACTIVE
            .as_ref()
            .is_some_and(|p| p.plan.target(Role::Title).is_some())
    }

    fn place(p: &Placement, index: usize, thread: HANDLE, name: &str) {
        let c = &p.caches[index];
        // SAFETY: a live thread handle, and masks and IDs from the OS. Only this thread is affected.
        let ok = unsafe {
            if p.hard {
                let (group, mask) = c.masks[0];
                let affinity = GROUP_AFFINITY {
                    Mask: mask,
                    Group: group,
                    ..Default::default()
                };
                SetThreadGroupAffinity(thread, &affinity, std::ptr::null_mut())
            } else {
                SetThreadSelectedCpuSets(thread, c.ids.as_ptr(), c.ids.len() as u32)
            }
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            tracing::warn!(name, masks = %c, %error, "thread placement failed");
        } else {
            PLACED.fetch_add(1, Relaxed);
            tracing::info!(name, masks = %c, "thread placement accepted");
        }
    }

    /// At worker entry. Rust's name includes pool indices; unnamed workers supply a label.
    pub fn worker(fallback: &str) {
        let Some(p) = ACTIVE.as_ref() else { return };
        let current = std::thread::current();
        let name = current.name().unwrap_or(fallback);
        let role = match name {
            "title-main" => Role::Title,
            "victory-submit" | "victory-relay" => Role::SubmitOrRelay,
            _ => Role::Helper,
        };
        if let Some(index) = p.plan.target(role) {
            // SAFETY: pseudo handle of this running thread.
            place(p, index, unsafe { GetCurrentThread() }, name);
        }
    }

    /// The two prep pool job entry points also exist on branches without the pool.
    /// The submitting thread can steal these jobs and must keep its own placement.
    pub(crate) fn prep_job() {
        thread_local! {
            static CHECKED: () = {
                if enabled() && std::thread::current().name().is_some_and(|n| n.starts_with("victory-prep-")) {
                    worker("prep");
                }
            };
        }
        CHECKED.with(|_| ());
    }

    pub fn title_created(thread: BorrowedHandle<'_>, id: u32) {
        if let Some(p) = ACTIVE.as_ref()
            && let Some(index) = p.plan.target(Role::Title)
        {
            place(
                p,
                index,
                thread.as_raw_handle(),
                &format!("title-CreateThread-{id}"),
            );
        }
    }

    pub(crate) fn log_interval() {
        if enabled() {
            tracing::info!(
                placed = PLACED.load(Relaxed),
                "core plan threads placed since start"
            );
        }
    }
}

#[cfg(windows)]
pub use win::{enabled, places_titles, title_created, worker};
#[cfg(windows)]
pub(crate) use win::{log_interval, prep_job};
#[cfg(not(windows))]
pub fn worker(_: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_route_every_role() {
        use Role::*;
        let a = Some(0);
        let b = Some(1);
        for (value, expected) in [
            ("ours-a", [a, a, None]),
            ("ours-b", [b, b, None]),
            ("split", [a, b, None]),
            ("title-a", [b, b, a]),
            ("together", [a, a, a]),
        ] {
            let plan = Plan::parse(Some(value)).unwrap();
            assert_eq!(
                [SubmitOrRelay, Helper, Title].map(|r| plan.target(r)),
                expected
            );
        }
        for value in [None, Some(""), Some("0"), Some("off"), Some("other")] {
            assert_eq!(Plan::parse(value), None);
        }
    }

    fn cache(mask: usize, mib: u32) -> Cache {
        Cache {
            bytes: mib << 20,
            masks: vec![(0, mask)],
            classes: vec![0; mask.count_ones() as usize],
            ..Default::default()
        }
    }

    #[test]
    fn selects_disjoint_equal_caches_in_processor_order() {
        let caches = [cache(0xfc0, 16), cache(0x3f, 16)];
        for hard in [false, true] {
            let [a, b] = pair(&caches, hard).unwrap();
            assert_eq!(
                (&a.masks[..], &b.masks[..]),
                (&[(0, 0x3f)][..], &[(0, 0xfc0)][..])
            );
        }
        assert!(pair(&caches[..1], false).is_none());
        assert!(pair(&[cache(3, 16), cache(12, 32)], false).is_none());
        assert!(pair(&[cache(3, 16), cache(6, 16)], false).is_none());
        let three = [cache(3, 32), cache(12, 16), cache(48, 16)];
        assert_eq!(pair(&three, false).unwrap().map(|c| c.masks[0].1), [12, 48]);
    }

    #[test]
    fn hybrid_groups_need_the_same_efficiency_class_mix() {
        let mut caches = [cache(3, 16), cache(12, 16)];
        caches[1].classes = vec![0, 1];
        assert!(pair(&caches, false).is_none());
        caches[0].classes = vec![0, 1];
        assert!(pair(&caches, false).is_some());
        caches[0].classes.pop();
        assert!(pair(&caches, false).is_none());
    }

    #[test]
    fn a_hard_mask_must_fit_in_one_windows_group() {
        let mut caches = [cache(3, 16), cache(12, 16)];
        caches[0].masks = vec![(0, 1), (1, 1)];
        assert!(pair(&caches, false).is_some());
        assert!(pair(&caches, true).is_none());
    }
}

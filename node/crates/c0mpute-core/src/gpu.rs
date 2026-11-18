//! Which accelerator this box has, for work that needs a real GPU.
//!
//! NVIDIA is whatever `nvidia-smi` reports. AMD counts only as a **discrete**
//! card with dedicated VRAM: integrated GPUs (APUs such as Barcelo/gfx90c)
//! carve their "VRAM" out of system RAM, ROCm does not support most of them,
//! and treating them as GPUs made boxes advertise `c0mpute:gpu:amd` and try
//! GPU runtimes that then ran on the CPU. An AMD card qualifies when sysfs
//! reports at least [`AMD_MIN_VRAM_MIB`] of dedicated VRAM and its KFD
//! topology node is not an APU (an APU's GPU node also carries CPU cores).

use std::path::Path;

/// Dedicated VRAM an AMD card needs to count as a GPU (APUs report a small
/// BIOS carve-out, typically 512 MiB to 2 GiB).
pub const AMD_MIN_VRAM_MIB: u64 = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    Nvidia,
    Amd,
}

impl GpuKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GpuKind::Nvidia => "nvidia",
            GpuKind::Amd => "amd",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gpu {
    pub kind: GpuKind,
    /// The largest card's dedicated memory.
    pub vram_mib: u64,
}

/// The box's usable GPU, probed once.
pub fn detect() -> Option<Gpu> {
    static GPU: std::sync::OnceLock<Option<Gpu>> = std::sync::OnceLock::new();
    *GPU.get_or_init(probe)
}

fn probe() -> Option<Gpu> {
    if let Some(vram_mib) = crate::video::nvidia_vram_mib() {
        return Some(Gpu { kind: GpuKind::Nvidia, vram_mib });
    }
    amd_discrete_vram_mib().map(|vram_mib| Gpu { kind: GpuKind::Amd, vram_mib })
}

/// One `/sys/class/kfd/kfd/topology/nodes/N/properties` file, the fields we use.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KfdNode {
    pub simd_count: u64,
    pub cpu_cores_count: u64,
}

pub fn parse_kfd_properties(text: &str) -> KfdNode {
    let mut n = KfdNode::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
        let v = v.parse::<u64>().unwrap_or(0);
        match k {
            "simd_count" => n.simd_count = v,
            "cpu_cores_count" => n.cpu_cores_count = v,
            _ => {}
        }
    }
    n
}

/// Decide from sysfs facts: `cards` are (PCI vendor, dedicated VRAM bytes) for
/// each DRM card, `kfd` the KFD topology nodes. Returns the largest discrete
/// AMD card's VRAM in MiB.
pub fn amd_discrete_from(cards: &[(u32, u64)], kfd: &[KfdNode]) -> Option<u64> {
    let gpu_nodes: Vec<&KfdNode> = kfd.iter().filter(|n| n.simd_count > 0).collect();
    // KFD lists every GPU; when all of them share a node with CPU cores the
    // only AMD GPUs here are integrated ones, whatever their carve-out says.
    if !gpu_nodes.is_empty() && gpu_nodes.iter().all(|n| n.cpu_cores_count > 0) {
        return None;
    }
    cards
        .iter()
        .filter(|(vendor, _)| *vendor == 0x1002)
        .map(|(_, bytes)| bytes / (1024 * 1024))
        .filter(|mib| *mib >= AMD_MIN_VRAM_MIB)
        .max()
}

/// Largest discrete AMD card's VRAM (MiB), from sysfs. Linux only.
pub fn amd_discrete_vram_mib() -> Option<u64> {
    let cards = drm_cards(Path::new("/sys/class/drm"));
    if cards.is_empty() {
        return None;
    }
    let kfd = kfd_nodes(Path::new("/sys/class/kfd/kfd/topology/nodes"));
    amd_discrete_from(&cards, &kfd)
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn drm_cards(dir: &Path) -> Vec<(u32, u64)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        // card0, card1 … (not card0-HDMI-A-1 connectors, not renderD*)
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = e.path().join("device");
        let Some(vendor) = read_trim(&dev.join("vendor")).and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()) else {
            continue;
        };
        let vram = read_trim(&dev.join("mem_info_vram_total")).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        out.push((vendor, vram));
    }
    out
}

fn kfd_nodes(dir: &Path) -> Vec<KfdNode> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("properties")).ok())
        .map(|t| parse_kfd_properties(&t))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const CPU_NODE: KfdNode = KfdNode { simd_count: 0, cpu_cores_count: 16 };

    #[test]
    fn apu_is_not_a_gpu() {
        // bonita: Barcelo APU, 2 GiB carve-out, its GPU node carries the CPU cores.
        let apu = KfdNode { simd_count: 32, cpu_cores_count: 16 };
        assert_eq!(amd_discrete_from(&[(0x1002, 2 * GIB)], &[apu]), None);
        // Even a big BIOS carve-out stays an APU.
        assert_eq!(amd_discrete_from(&[(0x1002, 8 * GIB)], &[apu]), None);
        // No KFD at all and only a small carve-out (dev2-style): not a GPU.
        assert_eq!(amd_discrete_from(&[(0x1002, 512 * 1024 * 1024)], &[]), None);
    }

    #[test]
    fn discrete_card_is_a_gpu() {
        let dgpu = KfdNode { simd_count: 96, cpu_cores_count: 0 };
        assert_eq!(amd_discrete_from(&[(0x1002, 16 * GIB)], &[CPU_NODE, dgpu]), Some(16 * 1024));
        // APU + discrete card in one box: the discrete card counts.
        let apu = KfdNode { simd_count: 8, cpu_cores_count: 8 };
        assert_eq!(amd_discrete_from(&[(0x1002, GIB), (0x1002, 24 * GIB)], &[apu, dgpu]), Some(24 * 1024));
        // Non-AMD vendors are not this function's business.
        assert_eq!(amd_discrete_from(&[(0x10de, 24 * GIB)], &[CPU_NODE]), None);
    }

    #[test]
    fn kfd_properties_parse() {
        let n = parse_kfd_properties("cpu_cores_count 0\nsimd_count 120\nmem_banks_count 1\ngfx_target_version 110000\n");
        assert_eq!(n, KfdNode { simd_count: 120, cpu_cores_count: 0 });
    }
}

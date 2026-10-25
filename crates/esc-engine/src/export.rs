//! 把映射表导出成传统的静态配置，让面板调好的东西可以直接交给
//! `esc hub --config` / `esc spoke --config` 跑——面板不是必需品，
//! 而是同一份事实的另一种编辑方式。

use serde::Serialize;

use esc_common::config::{HubSection, HubService, SpokeSection, SpokeService, TlsConfig};
use esc_control_api::{ExportedConfig, Mapping, Role};

#[derive(Serialize)]
struct HubFile<'a> {
    hub: &'a HubSection,
    tls: &'a TlsConfig,
    #[serde(rename = "service")]
    service: Vec<HubService>,
}

#[derive(Serialize)]
struct SpokeFile<'a> {
    spoke: &'a SpokeSection,
    tls: &'a TlsConfig,
    #[serde(rename = "service")]
    service: Vec<SpokeService>,
}

/// 生成一份 `esc hub` 可以直接吃的 TOML。
pub fn export_hub(mappings: &[Mapping], hub: &HubSection, tls: &TlsConfig) -> ExportedConfig {
    let service: Vec<HubService> = mappings
        .iter()
        .filter(|m| m.enabled)
        .filter_map(|m| {
            m.listen.as_ref().map(|listen| HubService {
                name: m.name.clone(),
                listen: listen.clone(),
                spoke: m.spoke.clone(),
            })
        })
        .collect();
    let file = HubFile { hub, tls, service };
    ExportedConfig {
        role: Role::Hub,
        filename: "hub.toml".into(),
        toml: with_header(&file, "esc hub"),
    }
}

/// 生成一份 `esc spoke` 可以直接吃的 TOML。
pub fn export_spoke(mappings: &[Mapping], spoke: &SpokeSection, tls: &TlsConfig) -> ExportedConfig {
    let service: Vec<SpokeService> = mappings
        .iter()
        .filter(|m| m.enabled)
        .filter_map(|m| {
            m.local.as_ref().map(|local| SpokeService {
                name: m.name.clone(),
                local: local.clone(),
            })
        })
        .collect();
    let file = SpokeFile { spoke, tls, service };
    ExportedConfig {
        role: Role::Spoke,
        filename: "spoke.toml".into(),
        toml: with_header(&file, "esc spoke"),
    }
}

fn with_header<T: Serialize>(value: &T, what: &str) -> String {
    let body = toml::to_string_pretty(value).unwrap_or_default();
    format!(
        "# 由 esc 面板导出（{what}），可直接用 `{what} --config <本文件>` 启动。\n\
         # 注意：本文件是快照，后续改动请回到面板或映射表，两边不会自动同步。\n\n{body}"
    )
}

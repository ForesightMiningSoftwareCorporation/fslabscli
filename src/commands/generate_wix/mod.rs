use crate::commands::check_workspace::{
    check_workspace, Options as CheckWorkspaceOptions, Result as Member,
};
use crate::PrettyPrintable;
use clap::Parser;
use convert_case::{Case, Casing};
use indexmap::IndexMap;
use quick_xml::events::{BytesPI, BytesStart, Event};
use quick_xml::Writer;
use serde::Serialize;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use std::fs;

#[derive(Debug, Parser)]
#[command(about = "Generate wix manifest for a launcher")]
pub struct Options {
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Serialize)]
pub struct GenerateResult {
    pub wix_files: IndexMap<String, Wix>,
}

impl Display for GenerateResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "")
    }
}

impl PrettyPrintable for GenerateResult {
    fn pretty_print(&self) -> String {
        self.wix_files
            .iter()
            .filter_map(|(_, wix)| wix.to_xml().ok())
            .collect::<Vec<String>>()
            .join("\n\n")
    }
}

// Wix File structure
#[derive(Serialize, Debug)]
pub struct Wix {
    xmlns: String,
    defines: IndexMap<String, String>,
    product: Product,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Product {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Name")]
    pub name: String,
    #[serde(rename = "@Language")]
    pub language: String,
    #[serde(rename = "@Version")]
    pub version: String,
    #[serde(rename = "@Manufacturer")]
    pub manufacturer: String,
    #[serde(rename = "@UpgradeCode")]
    pub upgrade_code: String,
    pub package: Package,
    pub major_upgrade: MajorUpgrade,
    pub media: Media,
    pub directory: Vec<Directory>,
    pub feature: Feature,
    pub icon: Icon,
    pub property: Vec<WixVariable>,
    #[serde(rename = "UIRef")]
    pub ui_ref: UIRef,
    pub wix_variable: Vec<WixVariable>,
}

impl Default for Product {
    fn default() -> Self {
        Self {
            id: "*".to_string(),
            name: "$(var.ProductName)".to_string(),
            language: "1033".to_string(),
            version: "$(var.AppVersion)".to_string(),
            manufacturer: "$(var.Manufacturer)".to_string(),
            upgrade_code: "$(var.UpgradeCode)".to_string(),
            package: Package::default(),
            major_upgrade: MajorUpgrade::default(),
            media: Media::default(),
            feature: Feature::default(),
            icon: Icon::default(),
            ui_ref: UIRef::default(),
            property: vec![WixVariable::new(
                "ARPPRODUCTICON".to_string(),
                "ProductIcon".to_string(),
            )],
            wix_variable: vec![
                WixVariable::new(
                    "WixUILicenseRtf".to_string(),
                    "assets\\eula.rtf".to_string(),
                ),
                WixVariable::new(
                    "WixUIBannerBmp".to_string(),
                    "assets\\banner.bmp".to_string(),
                ),
                WixVariable::new(
                    "WixUIDialogBmp".to_string(),
                    "assets\\dialog.bmp".to_string(),
                ),
            ],
            directory: vec![],
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Package {
    #[serde(rename = "@Description")]
    pub description: String,
    #[serde(rename = "@Manufacturer")]
    pub manufacturer: String,
    #[serde(rename = "@InstallerVersion")]
    pub installer_version: String,
    #[serde(rename = "@Compressed")]
    pub compressed: String,
    #[serde(rename = "@InstallScope")]
    pub install_scope: String,
    #[serde(rename = "@InstallPrivileges")]
    pub install_privileges: String,
}

impl Default for Package {
    fn default() -> Self {
        Self {
            description: "$(var.AppDescription)".to_string(),
            manufacturer: "$(var.Manufacturer)".to_string(),
            installer_version: "200".to_string(),
            compressed: "yes".to_string(),
            install_scope: "perUser".to_string(),
            install_privileges: "limited".to_string(),
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct MajorUpgrade {
    #[serde(rename = "@Schedule")]
    pub schedule: String,
    #[serde(rename = "@AllowDowngrades")]
    pub allow_downgrades: String,
}

impl Default for MajorUpgrade {
    fn default() -> Self {
        Self {
            schedule: "afterInstallInitialize".to_string(),
            allow_downgrades: "yes".to_string(),
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Media {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Cabinet")]
    pub cabinet: String,
    #[serde(rename = "@EmbedCab")]
    pub embed_cab: String,
}

impl Default for Media {
    fn default() -> Self {
        Self {
            id: "1".to_string(),
            cabinet: "product.cab".to_string(),
            embed_cab: "yes".to_string(),
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Feature {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Title")]
    pub title: String,
    #[serde(rename = "@Level")]
    pub level: String,
    pub component_ref: Vec<ComponentRef>,
}

impl Default for Feature {
    fn default() -> Self {
        Self {
            id: "Required".to_string(),
            title: "Required".to_string(),
            level: "1".to_string(),
            component_ref: vec![
                ComponentRef::new("LauncherBinary".to_string()),
                ComponentRef::new("LaunchSettings".to_string()),
                ComponentRef::new("AppBinary".to_string()),
                ComponentRef::new("ApplicationShortcut".to_string()),
                ComponentRef::new("CreateAppDataFolder".to_string()),
                ComponentRef::new("CreateUpdatesFolder".to_string()),
                ComponentRef::new("CreateLicensesFolder".to_string()),
                ComponentRef::new("CreateCacheFolder".to_string()),
                ComponentRef::new("CreateLogsFolder".to_string()),
                ComponentRef::new("CreateBlastIqLogsFolder".to_string()),
            ],
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct ComponentRef {
    #[serde(rename = "@Id")]
    pub id: String,
}

impl ComponentRef {
    fn new(id: String) -> Self {
        Self { id }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Icon {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@SourceFile")]
    pub source_file: String,
}

impl Default for Icon {
    fn default() -> Self {
        Self {
            id: "ProductIcon".to_string(),
            source_file: "..\\assets\\icons\\BlastIQ_icon.ico".to_string(),
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct UIRef {
    #[serde(rename = "@Id")]
    pub id: String,
}

impl Default for UIRef {
    fn default() -> Self {
        Self {
            id: "WixUI_Minimal".to_string(),
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct WixVariable {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Value")]
    pub value: String,
}

impl WixVariable {
    fn new(id: String, value: String) -> Self {
        Self { id, value }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Directory {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "Component")]
    pub components: Vec<DirectoryComponent>,
    #[serde(rename = "Directory")]
    pub directories: Vec<Directory>,
}

impl Directory {
    fn new(id: String, name: Option<String>) -> Self {
        Self {
            id,
            name,
            directories: vec![],
            components: vec![],
        }
    }

    fn add_subdir(&mut self, other: Self) {
        self.directories.push(other);
    }

    fn add_component(&mut self, component: DirectoryComponent) {
        self.components.push(component);
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct DirectoryComponent {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Guid")]
    pub guid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_folder: Option<CreateFolder>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<DirectoryComponentFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_key: Option<RegistryKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_value: Option<RegistryValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove_folder: Option<RemoveObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<Shortcut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove_file: Option<RemoveObject>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct CreateFolder {}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct DirectoryComponentFile {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Name")]
    pub name: String,
    #[serde(rename = "@Source")]
    pub source: String,
    #[serde(rename = "@Checksum")]
    pub checksum: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Shortcut {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Name")]
    pub name: String,
    #[serde(rename = "@Description")]
    pub description: String,
    #[serde(rename = "@Target")]
    pub target: String,
    #[serde(rename = "@WorkingDirectory")]
    pub working_directory: String,
    #[serde(rename = "@Icon")]
    pub icon: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct RemoveObject {
    #[serde(rename = "@Id")]
    pub id: String,
    #[serde(rename = "@Name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "@On")]
    pub on: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct RegistryKey {
    #[serde(rename = "@Root")]
    pub root: String,
    #[serde(rename = "@Key")]
    pub key: String,
    pub registry_value: RegistryValue,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct RegistryValue {
    #[serde(rename = "@Root", skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(rename = "@Key", skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(rename = "@Name")]
    pub name: String,
    #[serde(rename = "@Value")]
    pub value: String,
    #[serde(rename = "@Type")]
    pub value_type: String,
    #[serde(rename = "@KeyPath")]
    pub key_path: String,
}

impl Wix {
    pub fn to_xml(&self) -> anyhow::Result<String> {
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
        // Xml PI
        writer.write_event(Event::PI(BytesPI::new("xml version=\"1.0\"")))?;
        let wix_start = BytesStart::from_content(format!("Wix xmlns=\"{}\"", self.xmlns), 3);
        let wix_end = wix_start.to_end();
        // Open Wix elem
        writer.write_event(Event::Start(wix_start.clone()))?;
        // Add defines rule
        self.defines.clone().into_iter().for_each(|(k, v)| {
            let _ = writer.write_event(Event::PI(BytesPI::new(format!("define {}=\"{}\"", k, v))));
        });
        writer.write_serializable("Product", &self.product)?;
        writer.write_event(Event::End(wix_end))?;
        Ok(std::str::from_utf8(&writer.into_inner())?.to_string())
    }
    fn new(member: &Member) -> Self {
        let defines = IndexMap::from([
            ("AppVersion", "{{APP_VERSION}}"),
            ("ProductName", "{{APP_NAME}}"),
            ("CrateName", &member.package),
            ("ProdNameForPath", "{{APP_PATH_NAME}}"),
            ("FallbackBinary", "{{FALLBACK_BINARY}}"),
            ("UpgradeCode", "{{UPGRADE_CODE}}"),
            ("GuidPrefix", "{{GUID_PREFIX}}"),
            ("MfgForPath", "Orica Digital"),
            ("Manufacturer", "Orica Australia Pty. Limited"),
            ("AppDescription", "Blast Design Software"),
            (
                "AppDescriptionLong",
                "Blast design, modelling, optimisation, and reporting.",
            ),
        ])
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let mut product: Product = Product {
            ..Default::default()
        };
        let mut root_directory =
            Directory::new("TARGETDIR".to_string(), Some("SourceDir".to_string()));

        // Start Menu Directory
        let mut application_programs_folder = Directory::new(
            "ApplicationProgramsFolder".to_string(),
            Some("$(var.ProductName)".to_string()),
        );
        application_programs_folder.add_component(DirectoryComponent {
            id: "ApplicationShortcut".to_string(),
            guid: "$(var.GuidPrefix)-e33a-42d9-9cec-5505163567a8".to_string(),
            shortcut: Some(Shortcut {
                id: "ApplicationStartMenuShortcut".to_string(),
                name: "$(var.ProductName)".to_string(),
                description: "$(var.AppDescriptionLong)".to_string(),
                target: "[!LauncherBinary]".to_string(),
                working_directory: "SettingsLocation".to_string(),
                icon: "ProductIcon".to_string(),
            }),
            remove_folder: Some(RemoveObject {
                id: "CleanUpShortCut".to_string(),
                name: None,
                on: "uninstall".to_string(),
            }),
            remove_file: None,
            registry_value: Some(RegistryValue {
                root: Some("HKCU".to_string()),
                key: Some("Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string()),
                name: "installed".to_string(),
                value_type: "integer".to_string(),
                value: "1".to_string(),
                key_path: "yes".to_string(),
            }),
            registry_key: None,
            file: None,
            create_folder: None,
        });
        let mut program_menu_folder = Directory::new("ProgramMenuFolder".to_string(), None);
        program_menu_folder.add_subdir(application_programs_folder);

        // Add Local AppData
        let mut local_app_data_folder = Directory::new("LocalAppDataFolder".to_string(), None);
        let mut data_location_folder = Directory::new(
            "DataLocation".to_string(),
            Some("$(var.ProductName)".to_string()),
        );
        data_location_folder.add_component(DirectoryComponent {
            id: "LauncherBinary".to_string(),
            guid: "$(var.GuidPrefix)-bbb7-4d99-ad53-e0eabc1cd732".to_string(),
            shortcut: None,
            remove_folder: Some(RemoveObject {
                id: "RemoveDataLocationDir".to_string(),
                on: "both".to_string(),
                name: None,
            }),
            remove_file: Some(RemoveObject {
                id: "PurgeLocalAppData".to_string(),
                name: Some("*.*".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "localappdata".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: Some(DirectoryComponentFile {
                id: "LauncherBinary".to_string(),
                name: "$(var.ProdNameForPath)_launcher.exe".to_string(),
                source: format!(
                    "..\\target\\x86_64-pc-windows-msvc\\release\\{}_launcher.exe",
                    member.package
                )
                .to_string(),
                checksum: "yes".to_string(),
            }),
            create_folder: None,
        });

        let mut binary_folder = Directory::new("binary".to_string(), Some("binary".to_string()));
        binary_folder.add_component(DirectoryComponent {
            id: "AppBinary".to_string(),
            guid: "$(var.GuidPrefix)-8a65-417b-82bf-20243e82f1ea".to_string(),
            shortcut: None,
            remove_folder: Some(RemoveObject {
                id: "RemoveBinaryDir".to_string(),
                on: "both".to_string(),
                name: None,
            }),
            remove_file: Some(RemoveObject {
                id: "PurgeBinary".to_string(),
                name: Some("*.*".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "localappdataversions".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: Some(DirectoryComponentFile {
                id: "AppBinary".to_string(),
                name: "$(var.FallbackBinary).exe".to_string(),
                source: "..\\target\\x86_64-pc-windows-msvc\\release\\$(var.CrateName).exe"
                    .to_string(),
                checksum: "yes".to_string(),
            }),
            create_folder: None,
        });
        data_location_folder.add_subdir(binary_folder);

        let mut update_folder =
            Directory::new("UpdatesLocation".to_string(), Some("updates".to_string()));
        update_folder.add_component(DirectoryComponent {
            id: "CreateUpdatesFolder".to_string(),
            guid: "$(var.GuidPrefix)-9f15-4fcd-ac41-931a1a2ae18c".to_string(),
            shortcut: None,
            remove_folder: Some(RemoveObject {
                id: "RemoveUpdatesLocationDir".to_string(),
                on: "both".to_string(),
                name: None,
            }),
            remove_file: Some(RemoveObject {
                id: "PurgeUpdates".to_string(),
                name: Some("*.*".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "updates".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: None,
            create_folder: Some(CreateFolder {}),
        });
        data_location_folder.add_subdir(update_folder);

        let mut license_folder =
            Directory::new("LicensesLocation".to_string(), Some("licenses".to_string()));
        license_folder.add_component(DirectoryComponent {
            id: "CreateLicensesFolder".to_string(),
            guid: "$(var.GuidPrefix)-d206-4f89-bddc-d209a8a176bc".to_string(),
            shortcut: None,
            remove_folder: Some(RemoveObject {
                id: "RemoveLicensesLocationDir".to_string(),
                on: "both".to_string(),
                name: None,
            }),
            remove_file: Some(RemoveObject {
                id: "PurgeLicenses".to_string(),
                name: Some("*.*".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "licenses".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: None,
            create_folder: Some(CreateFolder {}),
        });
        data_location_folder.add_subdir(license_folder);

        let mut cache_folder =
            Directory::new("CacheLocation".to_string(), Some("cache".to_string()));
        cache_folder.add_component(DirectoryComponent {
            id: "CreateCacheFolder".to_string(),
            guid: "$(var.GuidPrefix)-2412-4989-821d-958b35bc0608".to_string(),
            shortcut: None,
            remove_folder: Some(RemoveObject {
                id: "RemoveCacheLocationDir".to_string(),
                on: "both".to_string(),
                name: None,
            }),
            remove_file: Some(RemoveObject {
                id: "PurgeCache".to_string(),
                name: Some("*.*".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "cache".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: None,
            create_folder: Some(CreateFolder {}),
        });
        data_location_folder.add_subdir(cache_folder);

        // Add subapps if needed
        if !member.publish_detail.binary.installer.sub_apps.is_empty() {
            product.feature.component_ref.push(ComponentRef {
                id: "CreateSubAppsFolder".to_string(),
            });

            let mut sub_app_folder =
                Directory::new("SubAppsDirectory".to_string(), Some("apps".to_string()));
            sub_app_folder.add_component(DirectoryComponent {
                id: "CreateSubAppsFolder".to_string(),
                guid: "$(var.GuidPrefix)-91e9-4d3f-9b89-8e1735eca3d2".to_string(),
                shortcut: None,
                remove_folder: Some(RemoveObject {
                    id: "RemoveSubAppsDir".to_string(),
                    on: "both".to_string(),
                    name: None,
                }),
                remove_file: Some(RemoveObject {
                    id: "PurgeSubApps".to_string(),
                    name: Some("*.*".to_string()),
                    on: "both".to_string(),
                }),
                registry_key: Some(RegistryKey {
                    root: "HKCU".to_string(),
                    key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                    registry_value: RegistryValue {
                        root: None,
                        key: None,
                        name: "subapps".to_string(),
                        value_type: "string".to_string(),
                        value: "1".to_string(),
                        key_path: "yes".to_string(),
                    },
                }),
                registry_value: None,
                file: None,
                create_folder: Some(CreateFolder {}),
            });
            for (subapp, subapp_config) in &member.publish_detail.binary.installer.sub_apps {
                if let Some(guid_suffix) = subapp_config.guid_suffix.clone() {
                    let sub_app_key = subapp.to_case(Case::Pascal);
                    product.feature.component_ref.push(ComponentRef {
                        id: format!("{}Binary", sub_app_key).to_string(),
                    });
                    sub_app_folder.add_component(DirectoryComponent {
                        id: format!("{}Binary", sub_app_key).to_string(),
                        guid: format!("$(var.GuidPrefix)-{}", guid_suffix).to_string(),
                        shortcut: None,
                        remove_folder: None,
                        remove_file: None,
                        registry_key: Some(RegistryKey {
                            root: "HKCU".to_string(),
                            key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                            registry_value: RegistryValue {
                                root: None,
                                key: None,
                                name: format!("{}_binary", subapp).to_string(),
                                value_type: "string".to_string(),
                                value: "1".to_string(),
                                key_path: "yes".to_string(),
                            },
                        }),
                        registry_value: None,
                        file: Some(DirectoryComponentFile {
                            id: format!("{}Binary", sub_app_key).to_string(),
                            name: subapp.clone(),
                            source: format!(
                                "..\\target\\x86_64-pc-windows-msvc\\release\\{}.exe",
                                subapp
                            )
                            .to_string(),
                            checksum: "yes".to_string(),
                        }),
                        create_folder: None,
                    });
                }
            }
            data_location_folder.add_subdir(sub_app_folder);
        }

        local_app_data_folder.add_subdir(data_location_folder);
        // Roaming Directory
        // Add Local AppData
        let mut app_data_folder = Directory::new("AppDataFolder".to_string(), None);
        let mut settings_location_folder = Directory::new(
            "SettingsLocation".to_string(),
            Some("$(var.ProductName)".to_string()),
        );
        settings_location_folder.add_component(DirectoryComponent {
            id: "CreateAppDataFolder".to_string(),
            guid: "$(var.GuidPrefix)-c612-48f2-a68d-cf1374c77f65".to_string(),
            shortcut: None,
            remove_folder: Some(RemoveObject {
                id: "RemoveSettingsLocationDir".to_string(),
                on: "both".to_string(),
                name: None,
            }),
            remove_file: Some(RemoveObject {
                id: "PurgeAppData".to_string(),
                name: Some("*.*".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "appdata".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: None,
            create_folder: Some(CreateFolder {}),
        });
        settings_location_folder.add_component(DirectoryComponent {
            id: "LaunchSettings".to_string(),
            guid: "$(var.GuidPrefix)-c77c-4808-b929-07d22832b7fe".to_string(),
            shortcut: None,
            remove_folder: None,
            remove_file: Some(RemoveObject {
                id: "RemoveLaunchSettings".to_string(),
                name: Some("launch.json".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "launchsettings".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: Some(DirectoryComponentFile {
                id: "LaunchSettings".to_string(),
                name: "launch.json".to_string(),
                source: "assets\\launch.json".to_string(),
                checksum: "yes".to_string(),
            }),
            create_folder: None,
        });
        let mut log_location_folder =
            Directory::new("LogsLocation".to_string(), Some("logs".to_string()));
        log_location_folder.add_component(DirectoryComponent {
            id: "CreateLogsFolder".to_string(),
            guid: "$(var.GuidPrefix)-c18d-4c32-bfe3-a96d5e34c678".to_string(),
            shortcut: None,
            remove_folder: Some(RemoveObject {
                id: "RemoveLogsDir".to_string(),
                on: "both".to_string(),
                name: None,
            }),
            remove_file: Some(RemoveObject {
                id: "PurgeLogs".to_string(),
                name: Some("*.*".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "logs".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: None,
            create_folder: Some(CreateFolder {}),
        });
        let mut blastiq_log_folder = Directory::new(
            "BlastIqLogsLocation".to_string(),
            Some("blastiq".to_string()),
        );
        blastiq_log_folder.add_component(DirectoryComponent {
            id: "CreateBlastIqLogsFolder".to_string(),
            guid: "$(var.GuidPrefix)-495a-4767-952e-fb079b27df8d".to_string(),
            shortcut: None,
            remove_folder: Some(RemoveObject {
                id: "RemoveBlastIqLogsDir".to_string(),
                on: "both".to_string(),
                name: None,
            }),
            remove_file: Some(RemoveObject {
                id: "PurgeBlastIqLogs".to_string(),
                name: Some("*.*".to_string()),
                on: "both".to_string(),
            }),
            registry_key: Some(RegistryKey {
                root: "HKCU".to_string(),
                key: "Software\\$(var.MfgForPath)\\$(var.ProdNameForPath)".to_string(),
                registry_value: RegistryValue {
                    root: None,
                    key: None,
                    name: "blastiqlogs".to_string(),
                    value_type: "string".to_string(),
                    value: "1".to_string(),
                    key_path: "yes".to_string(),
                },
            }),
            registry_value: None,
            file: None,
            create_folder: Some(CreateFolder {}),
        });

        log_location_folder.add_subdir(blastiq_log_folder);

        settings_location_folder.add_subdir(log_location_folder);

        app_data_folder.add_subdir(settings_location_folder);

        root_directory.add_subdir(program_menu_folder);
        root_directory.add_subdir(local_app_data_folder);
        root_directory.add_subdir(app_data_folder);
        product.directory = vec![root_directory];
        Self {
            xmlns: "http://schemas.microsoft.com/wix/2006/wi".to_string(),
            defines,
            product,
        }
    }
}

pub async fn generate_wix(
    _options: Box<Options>,
    working_directory: PathBuf,
) -> anyhow::Result<GenerateResult> {
    // Get Workspace members that needs a wix file
    let members: Vec<Member> = check_workspace(
        Box::new(CheckWorkspaceOptions::new()),
        working_directory.clone(),
    )
    .await
    .map_err(|e| {
        log::error!("Unparseable template: {}", e);
        e
    })?
    .0
    .iter()
    .filter_map(|(_, v)| match v.publish_detail.binary.installer.publish {
        true => Some(v.clone()),
        false => None,
    })
    .collect();
    // For each member, generate wix file
    let wix_files: IndexMap<String, Wix> = members
        .iter()
        .map(|m| (m.package.clone(), Wix::new(m)))
        .collect();
    // For each member, write the wix file at path {relative_crate_path}/installer/installer.wxs
    members.into_iter().for_each(|m| {
        let wix_path = working_directory
            .join(m.path)
            .join("installer/installer.wxs");
        if let Some(wix_file) = wix_files.get(&m.package.clone()) {
            if let Ok(wix_xml) = wix_file.to_xml() {
                let _ = fs::write(wix_path, wix_xml);
            }
        }
    });
    Ok(GenerateResult { wix_files })
}

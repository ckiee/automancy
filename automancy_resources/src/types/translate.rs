use std::ffi::OsStr;
use std::fs::{read_dir, read_to_string};
use std::path::Path;

use hashbrown::HashMap;
use serde::Deserialize;

use automancy_defs::{
    id::{Id, SharedStr},
    parse_map_id_str,
};

use crate::{ResourceManager, RON_EXT};

#[derive(Debug, Default, Clone)]
pub struct TranslateDef {
    pub none: SharedStr,
    pub unnamed: SharedStr,

    pub(crate) items: HashMap<Id, SharedStr>,
    pub(crate) tiles: HashMap<Id, SharedStr>,
    pub(crate) categories: HashMap<Id, SharedStr>,
    pub(crate) scripts: HashMap<Id, SharedStr>,

    pub(crate) gui: HashMap<Id, SharedStr>,
    pub(crate) error: HashMap<Id, SharedStr>,
    pub(crate) research: HashMap<Id, SharedStr>,
    pub keys: HashMap<Id, SharedStr>,
}

#[derive(Debug, Deserialize)]
struct Raw {
    #[serde(default)]
    none: Option<String>,
    #[serde(default)]
    unnamed: Option<String>,

    #[serde(default)]
    items: HashMap<String, String>,
    #[serde(default)]
    tiles: HashMap<String, String>,
    #[serde(default)]
    categories: HashMap<String, String>,
    #[serde(default)]
    scripts: HashMap<String, String>,

    #[serde(default)]
    gui: HashMap<String, String>,
    #[serde(default)]
    error: HashMap<String, String>,
    #[serde(default)]
    research: HashMap<String, String>,
    #[serde(default)]
    keys: HashMap<String, String>,
}

impl ResourceManager {
    fn load_translate(&mut self, file: &Path, namespace: &str) -> anyhow::Result<()> {
        log::info!("Loading translate at: {file:?}");

        let v = ron::from_str::<Raw>(&read_to_string(file)?)?;

        let mut new = TranslateDef {
            none: SharedStr::default(),
            unnamed: SharedStr::default(),
            items: parse_map_id_str(v.items.into_iter(), &mut self.interner, Some(namespace)),
            tiles: parse_map_id_str(v.tiles.into_iter(), &mut self.interner, Some(namespace)),
            categories: parse_map_id_str(
                v.categories.into_iter(),
                &mut self.interner,
                Some(namespace),
            ),
            scripts: parse_map_id_str(v.scripts.into_iter(), &mut self.interner, Some(namespace)),
            gui: parse_map_id_str(v.gui.into_iter(), &mut self.interner, Some(namespace)),
            keys: parse_map_id_str(v.keys.into_iter(), &mut self.interner, Some(namespace)),
            error: parse_map_id_str(v.error.into_iter(), &mut self.interner, Some(namespace)),
            research: parse_map_id_str(v.research.into_iter(), &mut self.interner, Some(namespace)),
        };
        if let Some(v) = v.none {
            new.none = v.into();
        }
        if let Some(v) = v.unnamed {
            new.unnamed = v.into();
        }
        if self.translates.none.is_empty() {
            self.translates.none = new.none;
        }
        if self.translates.unnamed.is_empty() {
            self.translates.unnamed = new.unnamed;
        }
        self.translates.items.extend(new.items);
        self.translates.tiles.extend(new.tiles);
        self.translates.categories.extend(new.categories);
        self.translates.scripts.extend(new.scripts);
        self.translates.gui.extend(new.gui);
        self.translates.keys.extend(new.keys);
        self.translates.error.extend(new.error);
        self.translates.research.extend(new.research);

        Ok(())
    }

    pub fn load_translates(&mut self, dir: &Path, namespace: &str) -> anyhow::Result<()> {
        let translates = dir.join("translates");
        let translates = read_dir(translates);

        if let Ok(translates) = translates {
            for file in translates
                .into_iter()
                .flatten()
                .map(|v| v.path())
                .filter(|v| v.extension() == Some(OsStr::new(RON_EXT)))
            {
                // TODO language selection
                if file.file_stem() == Some(OsStr::new("en_US")) {
                    self.load_translate(&file, namespace)?;
                }
            }
        }

        Ok(())
    }

    pub fn item_name(&self, id: &Id) -> SharedStr {
        match self.translates.items.get(id) {
            Some(name) => name.clone(),
            None => self.translates.unnamed.clone(),
        }
    }

    pub fn try_item_name(&self, id: Option<&Id>) -> SharedStr {
        if let Some(id) = id {
            self.item_name(id)
        } else {
            self.translates.none.clone()
        }
    }

    pub fn script_name(&self, id: &Id) -> SharedStr {
        match self.translates.scripts.get(id) {
            Some(name) => name.clone(),
            None => self.translates.unnamed.clone(),
        }
    }

    pub fn try_script_name(&self, id: Option<&Id>) -> SharedStr {
        if let Some(id) = id {
            self.item_name(id)
        } else {
            self.translates.none.clone()
        }
    }

    pub fn tile_name(&self, id: &Id) -> SharedStr {
        match self.translates.tiles.get(id) {
            Some(name) => name.clone(),
            None => self.translates.unnamed.clone(),
        }
    }

    pub fn try_tile_name(&self, id: Option<&Id>) -> SharedStr {
        if let Some(id) = id {
            self.tile_name(id)
        } else {
            self.translates.none.clone()
        }
    }

    pub fn category_name(&self, id: &Id) -> SharedStr {
        match self.translates.categories.get(id) {
            Some(name) => name.clone(),
            None => self.translates.unnamed.clone(),
        }
    }

    pub fn try_category_name(&self, id: Option<&Id>) -> SharedStr {
        if let Some(id) = id {
            self.category_name(id)
        } else {
            self.translates.none.clone()
        }
    }

    pub fn gui_str(&self, id: &Id) -> SharedStr {
        match self.translates.gui.get(id) {
            Some(name) => name.clone(),
            None => self.translates.unnamed.clone(),
        }
    }

    pub fn research_str(&self, id: &Id) -> SharedStr {
        match self.translates.research.get(id) {
            Some(name) => name.clone(),
            None => self.translates.unnamed.clone(),
        }
    }
}

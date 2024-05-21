use std::collections::{HashMap, HashSet};

use crate::{
    prelude::{StrictPath, ENV_DEBUG},
    resource::{config::RootsConfig, manifest::Os},
    scan::{
        launchers::{heroic::find_prefix, LauncherGame},
        TitleFinder,
    },
};

pub mod library {
    use super::*;

    pub const PATH: &str = "sideload_apps/library.json";

    #[derive(serde::Deserialize)]
    pub struct Data {
        pub games: Vec<Game>,
    }

    #[derive(serde::Deserialize)]
    pub struct Game {
        pub app_name: String,
        pub title: String,
        pub install: Install,
        pub folder_name: Option<StrictPath>,
    }

    #[derive(serde::Deserialize)]
    pub struct Install {
        pub platform: Option<String>,
    }
}

pub fn scan(root: &RootsConfig, title_finder: &TitleFinder) -> HashMap<String, HashSet<LauncherGame>> {
    let mut out = HashMap::<String, HashSet<LauncherGame>>::new();

    for (app_id, game) in get_library(&root.path) {
        let raw_title = &game.title;

        let Some(official_title) = title_finder.find_one_by_normalized_name(raw_title) else {
            log::trace!("Ignoring unrecognized game: {}", raw_title);
            if std::env::var(ENV_DEBUG).is_ok() {
                eprintln!(
                    "Ignoring unrecognized game from Heroic/sideload: {} (app = {})",
                    raw_title, &app_id
                );
            }
            continue;
        };

        log::trace!(
            "Detected game: {} | app: {}, raw title: {}",
            &official_title,
            &app_id,
            raw_title
        );
        let platform = game.install.platform.as_deref();
        let prefix = find_prefix(&root.path, &game.title, platform, &game.app_name);
        out.entry(official_title).or_default().insert(LauncherGame {
            install_dir: game.folder_name,
            prefix,
            platform: platform.map(Os::from),
        });
    }

    out
}

pub fn get_library(source: &StrictPath) -> HashMap<String, library::Game> {
    let mut out = HashMap::new();

    let file = source.joined(library::PATH);

    let content = match file.try_read() {
        Ok(content) => content,
        Err(e) => {
            log::debug!("In sideload source '{:?}', unable to read library | {:?}", &file, e);
            return out;
        }
    };

    if let Ok(data) = serde_json::from_str::<library::Data>(&content) {
        for game in data.games {
            out.insert(game.app_name.clone(), game);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use velcro::{hash_map, hash_set};

    use super::*;
    use crate::{
        resource::{
            manifest::{Manifest, Os, Store},
            ResourceFile,
        },
        testing::repo,
    };

    fn manifest() -> Manifest {
        Manifest::load_from_string(
            r#"
            game-1:
              files:
                <base>/file1.txt: {}
            "#,
        )
        .unwrap()
    }

    fn title_finder() -> TitleFinder {
        TitleFinder::new(&Default::default(), &manifest(), Default::default())
    }

    #[test]
    fn scan_finds_all_games() {
        let root = RootsConfig {
            path: StrictPath::new(format!("{}/tests/launchers/heroic-sideload", repo())),
            store: Store::Heroic,
        };
        let games = scan(&root, &title_finder());
        assert_eq!(
            hash_map! {
                "game-1".to_string(): hash_set![LauncherGame {
                    install_dir: Some(StrictPath::new("/games/game-1".to_string())),
                    prefix: Some(StrictPath::new("/prefixes/game-1".to_string())),
                    platform: Some(Os::Windows),
                }],
            },
            games,
        );
    }
}

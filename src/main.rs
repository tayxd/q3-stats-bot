use anyhow::{bail, Result};
use clap::Parser;
use notify::{
    event::{AccessKind, AccessMode, CreateKind, ModifyKind, RenameMode},
    recommended_watcher, EventKind, Watcher,
};
use quick_xml::{events::Event, Reader};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use teloxide::prelude::*;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

static BANNED_STATS: [&str; 8] = [
    "MH",
    "RA",
    "YA",
    "GA",
    "Quad",
    "Haste",
    "Blue Flag",
    "Red Flag",
];

#[derive(Debug, Default)]
struct Weapon {
    name: String,
    hits: u32,
    shots: u32,
    kills: u32,
}

#[derive(Debug, Default)]
struct Player {
    name: String,
    stats: Vec<(String, String)>,
    weapons: Vec<Weapon>,
    items: Vec<(String, u32)>,
}

#[derive(Debug, Default)]
struct Team {
    score: String,
    players: Vec<Player>,
}

#[derive(Debug, Default)]
struct Match {
    map: String,
    match_type: String,
    duration: String,
    is_team_game: bool,
    teams: Vec<Team>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    folder_path: Option<String>,

    #[arg(short, long, allow_hyphen_values = true)]
    chat_id: Option<String>,

    #[arg(long)]
    test_file: Option<PathBuf>,
}

fn escape_markdown(message: &str) -> String {
    let mut escaped_message = String::new();
    for c in message.chars() {
        match c {
            '*' | '_' | '[' | ']' | '(' | ')' | '~' | '>' | '#' | '+' | '-' | '=' | '|' | '{'
            | '}' | '.' | '!' => {
                escaped_message.push('\\');
            }
            _ => {}
        }
        escaped_message.push(c);
    }
    escaped_message
}

fn format_duration(duration: &str) -> String {
    let Ok(total_seconds) = duration.parse::<u64>() else {
        return duration.to_string();
    };

    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes}:{seconds:02}")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(test_file) = args.test_file {
        let data = tokio::fs::read_to_string(&test_file).await?;
        let match_data = parse_content(data)?;
        println!("{}", format_match_report(&match_data));
        return Ok(());
    }

    dotenvy::dotenv().ok();
    pretty_env_logger::formatted_builder()
        .filter_level(log::LevelFilter::Info)
        .init();
    log::info!("Starting q3reportbot...");

    let folder_path = args
        .folder_path
        .ok_or_else(|| anyhow::anyhow!("--folder-path is required unless --test-file is used"))?;
    let chat_id_arg = args
        .chat_id
        .ok_or_else(|| anyhow::anyhow!("--chat-id is required unless --test-file is used"))?;
    let chat_id_val = chat_id_arg
        .parse::<i64>()
        .map_err(|e| anyhow::anyhow!("Failed to parse chat_id '{}': {}", chat_id_arg, e))?;
    let chat_id = ChatId(chat_id_val);
    let bot = Bot::from_env();

    log::info!("Monitoring folder: {}", folder_path);
    log::info!("Target chat ID: {}", chat_id_arg);

    monitor_folder(bot, chat_id, folder_path).await?;

    Ok(())
}

/// Messages handled by the monitor loop.
enum MonitorMsg {
    /// Raw event from the filesystem watcher.
    Fs(notify::Result<notify::Event>),
    /// A newly created directory that should be scanned for XML files
    /// (files can land in it before the watcher manages to watch it).
    ScanDir(PathBuf),
    /// A file to process after a delay (platforms without close-write events).
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    DelayedFile(PathBuf),
}

/// Delay before scanning a freshly created directory, so the server has time
/// to finish writing files that were created before the watch was added.
const NEW_DIR_SCAN_DELAY: Duration = Duration::from_millis(1000);

fn is_xml_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
}

/// Recursively collects all XML files under `dir`.
fn collect_xml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            log::error!("Unable to read directory {:?}: {}", dir, e);
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_xml_files(&path, out);
        } else if is_xml_file(&path) {
            out.push(path);
        }
    }
}

fn schedule_dir_scan(tx: &UnboundedSender<MonitorMsg>, dir: PathBuf) {
    let tx = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(NEW_DIR_SCAN_DELAY).await;
        let _ = tx.send(MonitorMsg::ScanDir(dir));
    });
}

async fn monitor_folder(bot: Bot, chat_id: ChatId, folder_path: String) -> Result<()> {
    let (tx, mut rx) = unbounded_channel::<MonitorMsg>();
    let watcher_tx = tx.clone();
    let mut watcher = recommended_watcher(move |res| {
        let _ = watcher_tx.send(MonitorMsg::Fs(res));
    })?;
    let path = Path::new(&folder_path);

    watcher.watch(path, notify::RecursiveMode::Recursive)?;

    log::info!("Watching for changes in {:?}", path);

    // Files already reported, so a file seen both by a directory scan and by
    // its own close event is sent only once.
    let mut processed: HashSet<PathBuf> = HashSet::new();

    while let Some(msg) = rx.recv().await {
        match msg {
            MonitorMsg::Fs(Ok(e)) => match e.kind {
                EventKind::Create(kind) => {
                    for p in e.paths {
                        let is_dir = matches!(kind, CreateKind::Folder)
                            || (matches!(kind, CreateKind::Any | CreateKind::Other) && p.is_dir());
                        if is_dir {
                            log::info!("New directory detected: {:?}", p);
                            schedule_dir_scan(&tx, p);
                        } else if !cfg!(target_os = "linux") && is_xml_file(&p) {
                            // no close-write events outside Linux (inotify):
                            // wait so the file is fully written
                            log::info!("New file detected: {:?}", p);
                            let tx = tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(NEW_DIR_SCAN_DELAY).await;
                                let _ = tx.send(MonitorMsg::DelayedFile(p));
                            });
                        }
                    }
                }
                // something moved into the watched tree (a directory or a finished file)
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                    for p in e.paths {
                        if p.is_dir() {
                            log::info!("Directory moved in: {:?}", p);
                            schedule_dir_scan(&tx, p);
                        } else if is_xml_file(&p) {
                            log::info!("File moved in: {:?}", p);
                            process_file(&bot, chat_id, &p, &mut processed).await;
                        }
                    }
                }
                EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
                    for p in e.paths {
                        if is_xml_file(&p) {
                            log::info!("New file detected: {:?}", p);
                            process_file(&bot, chat_id, &p, &mut processed).await;
                        }
                    }
                }
                _ => (),
            },
            MonitorMsg::Fs(Err(e)) => log::error!("Watcher error: {:?}", e),
            MonitorMsg::DelayedFile(p) => {
                process_file(&bot, chat_id, &p, &mut processed).await;
            }
            MonitorMsg::ScanDir(dir) => {
                let dir_clone = dir.clone();
                let files = tokio::task::spawn_blocking(move || {
                    let mut files = Vec::new();
                    collect_xml_files(&dir_clone, &mut files);
                    files.sort();
                    files
                })
                .await?;
                for p in files {
                    if !processed.contains(&p) {
                        log::info!("File found in new directory {:?}: {:?}", dir, p);
                        process_file(&bot, chat_id, &p, &mut processed).await;
                    }
                }
            }
        }
    }
    log::error!("Watcher channel closed");
    Ok(())
}

async fn process_file(bot: &Bot, chat_id: ChatId, fpath: &Path, processed: &mut HashSet<PathBuf>) {
    if processed.contains(fpath) {
        return;
    }
    let data = match tokio::fs::read_to_string(fpath).await {
        Ok(data) => data,
        Err(e) => {
            log::error!("Unable to read file {:?}: {}", fpath, e);
            return;
        }
    };
    let match_data = match parse_content(data) {
        Ok(m) => m,
        Err(e) => {
            // not marked as processed: a later close event may bring the full file
            log::error!("Error parsing content of {:?}: {}", fpath, e);
            return;
        }
    };
    processed.insert(fpath.to_path_buf());
    let msg = format_match_report(&match_data);
    if let Err(err) = bot
        .send_message(chat_id, msg)
        .parse_mode(teloxide::types::ParseMode::MarkdownV2)
        .await
    {
        log::error!("Failed to send message: {}", err);
    }
}

fn parse_content(data: String) -> Result<Match> {
    let mut reader = Reader::from_str(&data);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut game_match = Match::default();

    let mut current_team: Option<Team> = None;
    let mut current_player: Option<Player> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => bail!("Error at position {}: {:?}", reader.error_position(), e),
            Ok(Event::Eof) => break,

            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"match" => {
                    for attr in e.attributes().flatten() {
                        match attr.key.into_inner() {
                            b"map" => {
                                game_match.map = String::from_utf8_lossy(&attr.value).into_owned()
                            }
                            b"type" => {
                                game_match.match_type =
                                    String::from_utf8_lossy(&attr.value).into_owned()
                            }
                            b"duration" => {
                                game_match.duration =
                                    String::from_utf8_lossy(&attr.value).into_owned()
                            }
                            b"isTeamGame" => {
                                game_match.is_team_game = String::from_utf8_lossy(&attr.value)
                                    .parse()
                                    .unwrap_or(false)
                            }
                            _ => {}
                        }
                    }
                }
                b"team" => {
                    let mut team = Team::default();
                    for attr in e.attributes().flatten() {
                        if attr.key.into_inner() == b"score" {
                            team.score = String::from_utf8_lossy(&attr.value).into_owned();
                        }
                    }
                    current_team = Some(team);
                }
                b"player" => {
                    let mut player = Player::default();
                    for attr in e.attributes().flatten() {
                        if attr.key.into_inner() == b"name" {
                            player.name = String::from_utf8_lossy(&attr.value).into_owned();
                        }
                    }
                    current_player = Some(player);
                }
                _ => {}
            },

            Ok(Event::End(e)) => match e.name().local_name().as_ref() {
                b"team" => {
                    if let Some(team) = current_team.take() {
                        game_match.teams.push(team);
                    }
                }
                b"player" => {
                    if let Some(player) = current_player.take() {
                        if let Some(team) = current_team.as_mut() {
                            team.players.push(player);
                        } else {
                            // player outside of a team (e.g. 1v1 or ffa)
                            let mut team = Team::default();
                            if let Some((_, score)) =
                                player.stats.iter().find(|(n, _)| n == "Score")
                            {
                                team.score = score.clone();
                            }
                            team.players.push(player);
                            game_match.teams.push(team);
                        }
                    }
                }
                _ => {}
            },

            Ok(Event::Empty(e)) => {
                let mut attr_map = std::collections::HashMap::new();
                for attr in e.attributes().flatten() {
                    attr_map.insert(attr.key.into_inner().to_vec(), attr.value.to_vec());
                }

                match e.name().as_ref() {
                    b"stat" => {
                        if let (Some(name_bytes), Some(val_bytes)) = (
                            attr_map.get(b"name".as_ref()),
                            attr_map.get(b"value".as_ref()),
                        ) {
                            let name = String::from_utf8_lossy(name_bytes).into_owned();
                            let val = String::from_utf8_lossy(val_bytes).into_owned();
                            if !BANNED_STATS.contains(&name.as_str()) {
                                if let Some(player) = current_player.as_mut() {
                                    player.stats.push((name, val));
                                }
                            }
                        }
                    }
                    b"weapon" => {
                        if let Some(name_bytes) = attr_map.get(b"name".as_ref()) {
                            let weapon = Weapon {
                                name: String::from_utf8_lossy(name_bytes).into_owned(),
                                hits: attr_map
                                    .get(b"hits".as_ref())
                                    .map(|b| String::from_utf8_lossy(b).parse().unwrap_or(0))
                                    .unwrap_or(0),
                                shots: attr_map
                                    .get(b"shots".as_ref())
                                    .map(|b| String::from_utf8_lossy(b).parse().unwrap_or(0))
                                    .unwrap_or(0),
                                kills: attr_map
                                    .get(b"kills".as_ref())
                                    .map(|b| String::from_utf8_lossy(b).parse().unwrap_or(0))
                                    .unwrap_or(0),
                            };
                            if let Some(player) = current_player.as_mut() {
                                player.weapons.push(weapon);
                            }
                        }
                    }
                    b"item" => {
                        if let (Some(name_bytes), Some(pickups_bytes)) = (
                            attr_map.get(b"name".as_ref()),
                            attr_map.get(b"pickups".as_ref()),
                        ) {
                            let name = String::from_utf8_lossy(name_bytes).into_owned();
                            let pickups: u32 =
                                String::from_utf8_lossy(pickups_bytes).parse().unwrap_or(0);
                            if let Some(player) = current_player.as_mut() {
                                player.items.push((name, pickups));
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => (),
        }
        buf.clear();
    }

    if game_match.map.is_empty() && game_match.teams.is_empty() {
        bail!("no output generated from XML");
    }

    Ok(game_match)
}

fn format_match_report(m: &Match) -> String {
    let mut output = String::new();
    output.push_str("*Match concluded*\n");
    output.push_str(&format!(
        "Map: {} \\| Type: {} \\| Duration: {}\n\n",
        escape_markdown(&m.map),
        escape_markdown(&m.match_type),
        escape_markdown(&format_duration(&m.duration))
    ));

    for (i, team) in m.teams.iter().enumerate() {
        if m.is_team_game {
            let team_label = if i == 0 { "Team One" } else { "Team Two" };
            output.push_str(&format!(
                "*{}*: *{}*\n",
                team_label,
                escape_markdown(&team.score)
            ));
        }

        for player in &team.players {
            output.push_str(&format!("```\nPlayer: {}\n", player.name));

            for (stat_name, stat_val) in &player.stats {
                output.push_str(&format!(
                    "{}: {}\n",
                    escape_markdown(stat_name),
                    escape_markdown(stat_val)
                ));
            }

            if !player.weapons.is_empty() {
                output.push_str("Weapons: \n");
                for w in &player.weapons {
                    let accuracy = if w.hits >= w.shots && w.hits > 0 {
                        100
                    } else if w.shots > 0 {
                        (w.hits * 100) / w.shots
                    } else {
                        0
                    };

                    output.push_str(&format!(
                        "{}: Shots: {} \\| Acc. {}% \\| Kills: {}\n",
                        escape_markdown(&w.name),
                        w.shots,
                        accuracy,
                        w.kills
                    ));
                }
            }

            let visible_items: Vec<&(String, u32)> =
                player.items.iter().filter(|(_, p)| *p > 0).collect();
            if !visible_items.is_empty() {
                let items_str: Vec<String> = visible_items
                    .iter()
                    .map(|(name, pickups)| format!("{}: {}", escape_markdown(name), pickups))
                    .collect();
                output.push_str(&format!("Items: \n{}\n", items_str.join(" \\| ")));
            }
            output.push_str("```\n");
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content() {
        let xml = std::fs::read_to_string("test.xml").expect("Unable to read test.xml");
        let result = parse_content(xml).unwrap();
        assert_eq!(result.map, "q3dm6");
        assert_eq!(result.match_type, "TDM");
        assert_eq!(result.duration, "601");
        assert_eq!(result.teams.len(), 2);

        // Team One (Score 5)
        assert_eq!(result.teams[0].score, "5");
        assert_eq!(result.teams[0].players.len(), 1);
        assert_eq!(result.teams[0].players[0].name, "Player1");
        assert_eq!(result.teams[0].players[0].weapons.len(), 2);

        // MG Accuracy: 13/29 = 44%
        let mg = &result.teams[0].players[0].weapons[0];
        assert_eq!(mg.name, "MG");
        assert_eq!(mg.hits, 13);
        assert_eq!(mg.shots, 29);
        let mg_acc = (mg.hits * 100) / mg.shots;
        assert_eq!(mg_acc, 44);

        // Team Two (Score 0)
        assert_eq!(result.teams[1].score, "0");
        assert_eq!(result.teams[1].players.len(), 2);
        assert_eq!(result.teams[1].players[0].name, "Player2");
        assert_eq!(result.teams[1].players[1].name, "Player3");

        let report = format_match_report(&result);
        assert!(report.contains("Duration: 10:01"));
    }

    #[test]
    fn test_is_xml_file() {
        assert!(is_xml_file(Path::new("/a/b/12_00_00.xml")));
        assert!(is_xml_file(Path::new("/a/b/GAME.XML")));
        assert!(!is_xml_file(Path::new("/a/b/game.log")));
        assert!(!is_xml_file(Path::new("/a/b/xml")));
    }

    #[test]
    fn test_collect_xml_files_nested() {
        let root = std::env::temp_dir().join(format!("q3bot_test_{}", std::process::id()));
        let day = root.join("2026").join("01").join("19");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("00_05_00.xml"), "x").unwrap();
        std::fs::write(day.join("notes.txt"), "x").unwrap();

        let mut files = Vec::new();
        collect_xml_files(&root.join("2026"), &mut files);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(files, vec![day.join("00_05_00.xml")]);
    }
}

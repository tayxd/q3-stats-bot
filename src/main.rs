use anyhow::{bail, Result};
use clap::Parser;
use notify::{recommended_watcher, EventKind, Watcher};
use quick_xml::{events::Event, Reader};
use std::{path::Path, sync::mpsc::channel};
use teloxide::prelude::*;

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
    folder_path: String,

    #[arg(short, long, allow_hyphen_values = true)]
    chat_id: String,
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

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    pretty_env_logger::formatted_builder()
        .filter_level(log::LevelFilter::Info)
        .init();
    log::info!("Starting q3reportbot...");

    let args = Args::parse();
    let chat_id_val = args
        .chat_id
        .parse::<i64>()
        .map_err(|e| anyhow::anyhow!("Failed to parse chat_id '{}': {}", args.chat_id, e))?;
    let chat_id = ChatId(chat_id_val);
    let bot = Bot::from_env();

    log::info!("Monitoring folder: {}", args.folder_path);
    log::info!("Target chat ID: {}", args.chat_id);

    monitor_folder(bot, chat_id, args.folder_path).await?;

    Ok(())
}

async fn monitor_folder(bot: Bot, chat_id: ChatId, folder_path: String) -> Result<()> {
    let (tx, rx) = channel();
    let mut watcher = recommended_watcher(tx)?;
    let path = Path::new(&folder_path);

    watcher.watch(path, notify::RecursiveMode::Recursive)?;

    log::info!("Watching for changes in {:?}", path);

    loop {
        match rx.recv() {
            Ok(event) => match event {
                Ok(e) => match e.kind {
                    EventKind::Create(_) => {
                        if let Some(fpath) = e.paths.last() {
                            if fpath.is_dir() {
                                continue;
                            }
                            log::info!("New file detected: {:?}", fpath);

                            // delay to ensure the file is fully written
                            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

                            match tokio::fs::read_to_string(fpath).await {
                                Ok(data) => match parse_content(data) {
                                    Ok(match_data) => {
                                        let msg = format_match_report(&match_data);
                                        if let Err(err) = bot
                                            .send_message(chat_id, msg)
                                            .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                                            .await
                                        {
                                            log::error!("Failed to send message: {}", err);
                                        }
                                    }
                                    Err(e) => {
                                        log::error!("Error parsing content: {}", e);
                                    }
                                },
                                Err(e) => log::error!("Unable to read file {:?}: {}", fpath, e),
                            }
                        }
                    }
                    _ => (),
                },
                Err(e) => log::error!("Watcher error: {:?}", e),
            },
            Err(e) => {
                log::error!("Channel error: {:?}", e);
                break;
            }
        }
    }
    Ok(())
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
        escape_markdown(&m.duration)
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
            output.push_str("```\n");
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_1v1() {
        let xml = std::fs::read_to_string("test2.xml").expect("Unable to read test2.xml");
        let result = parse_content(xml).unwrap();
        assert_eq!(result.map, "q3dm6");
        assert_eq!(result.match_type, "1v1");
        assert!(!result.is_team_game);
        assert_eq!(result.teams.len(), 2);
        assert_eq!(result.teams[0].players[0].name, "KDZ:VaNeZzz");
        assert_eq!(result.teams[0].score, "1");
    }

    #[test]
    fn test_parse_content() {
        let xml = std::fs::read_to_string("test.xml").expect("Unable to read test.xml");
        let result = parse_content(xml).unwrap();
        assert_eq!(result.map, "q3dm6");
        assert_eq!(result.match_type, "TDM");
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
    }
}

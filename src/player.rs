// =============== Imports ================
use crate::api;
use crate::config;
use crate::discord_rpc;
use crate::local_save;
use crate::mpvipc;
use crate::mpvipc::seek_to;
use crate::scraping;
use crate::skip_override;
use crate::utils;

use anyhow::{Context, Result};
use console::style;
use discord_rpc_client;
use reqwest::Client;
use std::{collections::HashMap, path::Path, time::Duration};
use tokio::time::sleep;
use tokio::{self, sync::mpsc};

pub async fn start_watching(
    client: &Client,
    id: i32,
    mal_id: i32,
    progress: u32,
    config: &config::Config,
    name: &String,
) -> Result<()> {
    let cur_ep = progress + 1;

    let (tx, mut rx) = mpsc::channel(1);

    let client_clone = client.clone();
    let config_lang = config.language.clone();
    let config_quality = config.quality.clone();
    let config_sub_or_dub = config.sub_or_dub.clone();
    let name_clone = name.clone();

    let override_setting = skip_override::search(id);
    let mut next_ep = cur_ep;
    if override_setting.filler {
        if !config.skip_filler {
            next_ep = filler(&client, mal_id, next_ep).await?;
        }
    } else if config.skip_filler {
        next_ep = filler(&client, mal_id, next_ep).await?;
    }

    log::info!(
        "Starting playback for anime: {}, Episode: {}",
        name,
        next_ep,
    );
    println!("Loading - {}, episode: {}", name, next_ep);

    tokio::task::spawn(async move {
        let mut next_url: Result<String> = Err(anyhow::anyhow!(""));
        while next_url.is_err() {
            tokio::time::sleep(Duration::from_secs(3)).await;
            next_url = get_url(
                &client_clone,
                &config_lang,
                mal_id,
                id,
                next_ep,
                &config_quality,
                &config_sub_or_dub,
                &name_clone,
            )
            .await
            .with_context(|| format!("Failed to fetch URL for episode {}", next_ep));
            if next_url.is_err() {
                eprintln!("Failed to get episode link, retrying...");
                log::warn!("Failed to get episode link for id: {}", id);
            }
        }
        tx.send(next_url.unwrap()).await.unwrap();
    });

    let mut player_args = config.player_args.split(' ').collect::<Vec<&str>>();
    player_args.retain(|arg| !arg.is_empty());
    let socket_path = format!("/tmp/yato-mpvsocket");
    if Path::new(&socket_path).exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
    let program = &config.player;
    let ipc_socket = format!("--input-ipc-server={}", socket_path);

    let url = rx.recv().await.unwrap();


    let _ = std::process::Command::new(program)
        .arg("--hwdec=auto")
        .arg("--quiet")
        .arg("--idle=yes")
        .arg("--force-window=yes")
        .arg(ipc_socket)
        .args(player_args)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to start player with program: {}", program))?;

    let db =
        local_save::ProgressDatabase::load().with_context(|| "Failed to load progress database")?;

    let entry = match db.get_entry(id) {
        Some(e) => e.to_owned(),
        None => local_save::WatchProgress {
            anilist_id: id,
            episode: cur_ep,
            position: 0.0,
            scraper_ids: {
                let mut map = HashMap::new();
                map.insert(config.language.to_string(), String::new());
                map
            },
        },
    };

    if cur_ep == entry.episode {
        let position = entry.position.round() as u64;
        let resuming_text = format!(
            "{:02}:{:02}:{:02}",
            position / 3600,
            (position % 3600) / 60,
            position % 60
        );
        println!("Resuming from - {}", style(resuming_text).bold());
    } else {
        println!("Starting from the begining");
    }

    Ok(())
}

pub async fn get_url(
    client: &Client,
    lang: &str,
    mal_id: i32,
    id: i32,
    episode: u32,
    quality: &str,
    sub_or_dub: &str,
    name: &String,
) -> Result<String> {
    let url: String = match lang {
        "hungarian" => {
            let url = scraping::hun_scraping::get_link(&client, mal_id, id, episode, quality).await;
            if url.is_err() {
                let err = format!("Anime not found on AnimeDrive, error: {}", url.unwrap_err());
                log::warn!("{}", err);
                return Err(anyhow::anyhow!(err));
            }
            url?
        }
        "english" => {
            let url = scraping::eng_scraping::get_link(
                &client, lang, id, episode, quality, sub_or_dub, name,
            )
            .await;
            if url.is_err() {
                let err = format!("Anime not found on AllAnime, error: {}", url.unwrap_err());
                log::warn!("{}", err);
                return Err(anyhow::anyhow!(err));
            }
            url?
        }
        _ => {
            // todo: Add more languages
            return Err(anyhow::anyhow!("Language not supported"));
        }
    };

    log::debug!("link: {}", url);
    Ok(url)
}

pub async fn filler(client: &Client, mal_id: i32, mut episode: u32) -> Result<u32> {
    loop {
        let response = api::jikan::filler(&client, mal_id, episode).await;
        if response.is_err() {
            break;
        } else if response.is_ok() && response? {
            log::info!("Skipping episode {} because it's a filler", episode);
            episode = episode + 1;
        } else {
            break;
        }
    }

    Ok(episode)
}

pub async fn watching(
    client: &Client,
    id: i32,
    mal_id: i32,
    cur_ep: u32,
    max_ep: u32,
    config: &config::Config,
    name: &String,
    syncing: bool,
    rpc_client: &mut discord_rpc_client::Client,
    cache: &mut HashMap<u32, String>,
) -> Result<bool> {
    while mpvipc::has_active_playback().await.is_err() {
        sleep(Duration::from_millis(250)).await;
    }

    let mut db = local_save::ProgressDatabase::load()?;

    let mut anime = api::aniskip::Anime {
        episode: cur_ep,
        mal_id: mal_id,
        skip_times: api::aniskip::SkipData::default(),
    };

    let skip_times = api::aniskip::get_and_parse_ani_skip_data(
        &client,
        anime.mal_id,
        anime.episode,
        2,
        &mut anime,
    )
    .await;

    match skip_times {
        Ok(_) => {
            api::aniskip::send_skip_times_to_mpv(&anime)
                .with_context(|| "Failed to send skip times to MPV")
                .unwrap();
        }
        Err(e) => {
            println!("Failed to fetch AniSkip data: {}", e);
        }
    }

    update_mpv_properties(&name, cur_ep)
        .await
        .with_context(|| "Failed to set properties")?;

    let override_setting = skip_override::search(id);

    let mut time_pos: f64 = 0.0;

    let anime_data = api::anilist::fetch::data_by_id(&client, id).await?;
    if config.discord_presence {
        let payload = discord_rpc::payload(&anime_data, cur_ep, max_ep, time_pos.round() as u64);
        rpc_client
            .set_activity(|_| payload)
            .expect("Failed to update activity");
        log::debug!("Set initial rpc");
    }

    while mpvipc::get_property("duration").await.is_err() {
        sleep(Duration::from_millis(250)).await;
    }
    let duration = mpvipc::get_property("duration").await?;
    let mut caching = false;
    let (tx, mut rx) = mpsc::channel(1);

    let entry = db.get_entry(id).unwrap();
    if entry.episode == cur_ep {
        seek_to(entry.position).await?
    }

    let end: bool;
    // * Main loop
    log::info!("Stating main loop.");
    loop {
        match mpvipc::has_active_playback().await {
            Ok(false) => {
                log::error!("End of episode");
                end = true;
                break;
            }
            Ok(true) => {}
            Err(_) => {
                log::error!("User exited");
                end = false;
                break;
            }
        }

        time_pos = mpvipc::get_property("time-pos").await?;

        let percent = time_pos / duration * 100.0;

        if percent > 70.0 && !caching && max_ep != cur_ep {
            caching = true;
            println!("Prefetching next episode.");
            
            let client_copy = client.clone();
            let config_copy = config.clone();
            let name_copy = name.clone();
            let tx = tx.clone();
            let mut next_ep = cur_ep + 1;

            if override_setting.filler {
                if !config.skip_filler {
                    next_ep = filler(&client, mal_id, next_ep).await?;
                }
            } else if config.skip_filler {
                next_ep = filler(&client, mal_id, next_ep).await?;
            }

            tokio::task::spawn(async move {
                let mut url = get_url(
                    &client_copy,
                    &config_copy.language,
                    mal_id,
                    id,
                    next_ep,
                    &config_copy.quality,
                    &config_copy.sub_or_dub,
                    &name_copy,
                )
                .await;

                while url.is_err() {
                    sleep(Duration::from_secs(3)).await;
                    url = get_url(
                        &client_copy,
                        &config_copy.language,
                        mal_id,
                        id,
                        next_ep,
                        &config_copy.quality,
                        &config_copy.sub_or_dub,
                        &name_copy,
                    )
                    .await;
                }
                tx.send(url.unwrap()).await.unwrap();

                println!("Next episode successfully fetched.");
            });
        }

        // Skipping intro and outro
        // Override basically does the opposite of the setting in the config file
        if override_setting.intro {
            if !config.skip_opening {
                if time_pos >= anime.skip_times.op.start && time_pos <= anime.skip_times.op.end {
                    mpvipc::seek_to(anime.skip_times.op.end)
                        .await
                        .with_context(|| "Failed to seek past opening")
                        .unwrap();
                    println!("Skipped intro");
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
            }
        } else {
            if time_pos >= anime.skip_times.op.start
                && time_pos <= anime.skip_times.op.end
                && config.skip_opening
            {
                mpvipc::seek_to(anime.skip_times.op.end)
                    .await
                    .with_context(|| "Failed to seek past opening")
                    .unwrap();
                println!("Skipped intro");
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
        if override_setting.outro {
            if !config.skip_credits {
                if time_pos >= anime.skip_times.ed.start && time_pos <= anime.skip_times.ed.end {
                    mpvipc::seek_to(anime.skip_times.ed.end)
                        .await
                        .with_context(|| "Failed to seek past credits")
                        .unwrap();
                    println!("Skipped outro");
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
            }
        } else {
            if time_pos >= anime.skip_times.ed.start
                && time_pos <= anime.skip_times.ed.end
                && config.skip_credits
            {
                mpvipc::seek_to(anime.skip_times.ed.end)
                    .await
                    .with_context(|| "Failed to seek past credits")
                    .unwrap();
                println!("Skipped outro");
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }

        // Skipping recap
        if override_setting.recap {
            if !config.skip_recap {
                if time_pos >= anime.skip_times.recap.start
                    && time_pos <= anime.skip_times.recap.end
                    && anime.skip_times.recap.end != 0.0
                {
                    mpvipc::seek_to(anime.skip_times.recap.end)
                        .await
                        .with_context(|| "Failed to seek past recap")
                        .unwrap();
                    println!("Skipped recap");
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
            }
        } else {
            if config.skip_recap {
                if time_pos >= anime.skip_times.recap.start
                    && time_pos <= anime.skip_times.recap.end
                    && anime.skip_times.recap.end != 0.0
                {
                    mpvipc::seek_to(anime.skip_times.recap.end)
                        .await
                        .with_context(|| "Failed to seek past recap")
                        .unwrap();
                    println!("Skipped recap");
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
            }
        }

        sleep(Duration::from_millis(250)).await;
    }

    utils::clear();

    if syncing {
        let scraper_id = db
            .get_scraper_id(id, &config.language)
            .map(|s| s.to_string())
            .unwrap();

        db.update_or_add(id, cur_ep, time_pos, &config.language, &scraper_id);
        db.save()
            .with_context(|| "Failed to save progress database")?;

        if time_pos / duration * 100.0 >= config.completion_time as f64 {
            api::anilist::mutation::update_progress(&client, id, cur_ep).await?;
            log::info!("Synced to anilist\n");
            println!("Synced to anilist");
        }
        log::info!("Saved progress for episode: {}", cur_ep);
    }

    if caching {
        cache.insert(cur_ep + 1, rx.recv().await.unwrap());
    }
    log::info!("Playback stopped for Episode: {} at {}", cur_ep, time_pos);

    Ok(end)
}

async fn update_mpv_properties(name: &str, cur_ep: u32) -> Result<()> {
    let title = format!("{} - Episode {}", name, cur_ep);
    mpvipc::set_property("title", &title).await?;
    mpvipc::set_property("force-media-title", &title).await?;
    Ok(())
}

// ! For better documentation readability, please use the following VScode extension:
// https://marketplace.visualstudio.com/items/?itemName=aaron-bond.better-comments

// * Meaning of the signs:
// ! - Important
// ? - Not in use yet
// * - Important, but not that important (usually just plain documentation, like what the function does)
// Without the signs, it's just a plain comment

// Achievements:
// Project started on 2025.03.27. by Nandor206
// First working version: 2025.04.10. (Hungarian only)
// Second working version: 2025.04.22. (Huge updates, English finally added, though the links from it don't work yet)
// First testings: 2025.04.24. (The code is so fast, that it quits before the video starts) - Fixed right away
// Discord rpc added! - The program is done, real men test in production
// Final version: 2025.04.27.

// =============== Imports ================
mod api;
mod args;
mod config;
mod discord_rpc;
mod local_save;
mod mpvipc;
mod player;
mod scraping;
mod skip_override;
mod theme;
mod utils;

use anyhow::{Context, Result};
use dialoguer::{Input, MultiSelect, Select};
use discord_rpc_client;
use reqwest::{Client, ClientBuilder};
use std::{collections::HashMap, default, io, process};
use tokio::{
    self,
    time::{Duration, sleep},
};

#[tokio::main]
async fn main() -> Result<()> {
    utils::init_log()?; // Initialize logging
    log::info!("Application started");

    // ! Creating client with a 120 second timeout (needed for hun_scraping, if they finally fix their site I will remove it)
    let client = ClientBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .with_context(|| "Failed to create HTTP client")?;
    let mut config = config::load_config();

    config::test(&config)?; // Testing if the config file is valid

    let (matches, rpc_client) = args::handle_args(&mut config, &client).await?;
    {
        utils::check_network(&client).await?; // Checking if the network is available

        log::debug!("Updated configuration: {:#?}", config);

        if matches.contains_id("anime") || matches.contains_id("number") {
            let anime_name = matches
                .get_one::<String>("anime")
                .map(String::as_str)
                .unwrap_or_else(|| "");
            let mut episode_number = matches
                .get_one::<u32>("number")
                .unwrap_or_else(|| &0)
                .to_owned();
            if episode_number > 0 {
                episode_number = episode_number - 1;
            }

            match anime_name.parse::<i32>() {
                Ok(anime_id) => {
                    let data = api::anilist::fetch::data_by_id(&client, anime_id).await?;
                    let max_ep = data.episodes;
                    let name = data.title;
                    let info = api::anilist::user_fetch::AnimeData {
                        id: anime_id,
                        title: name.clone(),
                        progress: episode_number,
                        episodes: max_ep,
                    };
                    watch(&client, config, rpc_client, info, false).await?;
                }
                Err(_) => {
                    let anime_id =
                        api::anilist::fetch::search(&client, anime_name.to_string()).await?;
                    let data = api::anilist::fetch::data_by_id(&client, anime_id).await?;
                    let max_ep = data.episodes;
                    let name = data.title;
                    let info = api::anilist::user_fetch::AnimeData {
                        id: anime_id,
                        title: name.clone(),
                        progress: episode_number,
                        episodes: max_ep,
                    };
                    watch(&client, config, rpc_client, info, false).await?;
                }
            }
            return Ok(());
        }

        api::anilist::user_fetch::check_credentials(&client).await?; // * Credentials are only needed after this part
        if matches.get_flag("new") {
            add_new_anime(&client).await?;
        }
    }

    // Clearing the screen for better looks
    utils::clear();

    let options = vec![
        "Continue Watching",
        "Edit (Episodes, Status, Score, Skipping)",
        "Info",
        "Add anime to list",
        "Exit",
    ];
    let theme = theme::CustomTheme {};

    let select_options = Select::with_theme(&theme)
        .with_prompt("Select an option:")
        .default(0)
        .items(&options)
        .interact_opt()?;

    // * Deciding what to do on each scenario
    if select_options.is_none() {
        // User pressed ESC or Q
        println!("See you later!");
        return Ok(());
    } else if select_options == Some(0) {
        if config.discord_presence {
            discord_rpc::selecting(&rpc_client, "Debating what to watch", "");
        }
        continue_watching(&client, config, rpc_client.clone()).await?;
    } else if select_options == Some(1) {
        // Edit (Episodes, Status, Score, Skipping)
        if config.discord_presence {
            discord_rpc::selecting(&rpc_client, "Updating their List", "");
        }
        update(&client).await?;
    } else if select_options == Some(2) {
        // Information about an anime
        info(&client).await?;
    } else if select_options == Some(3) {
        // Add new anime
        if config.discord_presence {
            discord_rpc::selecting(&rpc_client, "Thinking what to watch next", "");
        }
        add_new_anime(&client).await?;
    } else {
        // Exit
        utils::clear();
        println!("See you later!");
        return Ok(());
    }

    log::info!("Application exiting");
    Ok(())
}

async fn info(client: &Client) -> Result<()> {
    utils::clear();
    // * Input -> search for anime -> shows information of the selected one
    let theme = theme::CustomTheme {};
    let anime_name: String = Input::with_theme(&theme)
        .with_prompt("Enter the name of the anime")
        .interact_text()?;
    let search = api::anilist::fetch::search(&client, anime_name).await;
    utils::clear();
    if search.is_err() {
        eprintln!("Error: {}", search.unwrap_err());
        process::exit(1);
    } else {
        api::anilist::fetch::information(&client, search.unwrap()).await?;
    }

    Ok(())
}

async fn add_new_anime(client: &Client) -> Result<()> {
    utils::clear();
    // * Input -> search for anime -> add anime to the list
    let theme = theme::CustomTheme {};
    let anime_name: String = Input::with_theme(&theme)
        .with_prompt("Enter the name of the anime")
        .interact_text()?;

    utils::clear();

    let anime_id = api::anilist::fetch::search(&client, anime_name).await;
    match anime_id {
        Ok(anime_id) => {
            utils::clear();
            let options = vec![
                "Watching",
                "Completed",
                "Paused",
                "Dropped",
                "Planning",
                "Rewatching",
            ];
            let selection = Select::with_theme(&theme)
                .with_prompt("Select the status for the anime")
                .items(&options)
                .default(0)
                .interact_opt()?;

            if selection.is_none() {
                return Err(anyhow::anyhow!("No selection made"));
            }
            api::anilist::mutation::update_status(&client, anime_id, selection.unwrap()).await?;
        }
        Err(e) => {
            return Err(e);
        }
    }

    Ok(())
}

async fn continue_watching(
    client: &Client,
    config: config::Config,
    rpc_client: discord_rpc_client::Client,
) -> Result<()> {
    utils::clear();
    let info = api::anilist::user_fetch::current(&client).await?;
    utils::clear();

    watch(&client, config, rpc_client, info, true).await?;

    Ok(())
}

async fn update(client: &Client) -> Result<()> {
    utils::clear();
    let options = vec![
        "Change Progress",
        "Change Status",
        "Change Score",
        "Override Skip Settings",
    ];
    let theme = theme::CustomTheme {};
    let select_options = Select::with_theme(&theme)
        .with_prompt("Choose an option:")
        .default(0)
        .items(&options)
        .interact_opt()?;
    utils::clear();
    if select_options.is_none() {
        // User pressed ESC or Q
        return Err(anyhow::anyhow!("No selection was made"));
    } else if select_options == Some(0) {
        let anime_id = api::anilist::user_fetch::current(&client).await?.id;
        utils::clear();
        let new_episode: u32 = Input::new()
            .with_prompt("Enter a new episode number")
            .interact_text()?;
        utils::clear();
        api::anilist::mutation::update_progress(&client, anime_id, new_episode)
            .await
            .with_context(|| format!("Failed to update progress to episode {}", new_episode))?;
        println!("Progress updated!");
    } else if select_options == Some(1) {
        let anime_id = api::anilist::user_fetch::list_all(&client, 0).await?;
        utils::clear();
        let options = vec![
            "Watching",
            "Completed",
            "Paused",
            "Dropped",
            "Planning",
            "Rewatching",
        ];
        let selection = Select::with_theme(&theme)
            .with_prompt("Select the new status")
            .items(&options)
            .default(0)
            .interact_opt()?;
        api::anilist::mutation::update_status(&client, anime_id, selection.unwrap()).await?;
        // For rewatching and current the progress will be reset.
        if selection.unwrap() == 5 || selection.unwrap() == 0 {
            api::anilist::mutation::update_progress(&client, anime_id, 0).await?;
        }
    } else if select_options == Some(2) {
        let anime_id = api::anilist::user_fetch::list_all(&client, 1).await?;
        utils::clear();
        let theme = theme::CustomTheme {};
        let new_score: f64 = Input::with_theme(&theme)
            .with_prompt("Enter a new score on a scale of 1 to 10")
            .interact_text()
            .expect("Failed to read input, or input was invalid");
        utils::clear();
        if new_score < 1.0 || new_score > 10.0 {
            eprintln!("Score must be between 1 and 10");
            process::exit(1);
        } else {
            api::anilist::mutation::update_score(&client, anime_id, new_score).await?;
        }
    } else if select_options == Some(3) {
        if !skip_override::check_override() {
            println!(
                "Note: Overriding a config setting means doing the *opposite* of what's defined in your config file."
            );
            println!(
                "For example, if \"skip_opening\" is set to true in the config, the override setting will make it *not* skip the opening."
            );
            println!("\nUsage: Select with space, and press enter to confirm your selection.");
            println!("Press Enter to continue...");
            let _ = io::stdin().read_line(&mut String::new());
        }
        let options = vec!["Add/update override", "Delete an existing override"];
        let theme = theme::CustomTheme {};
        let over_ride = Select::with_theme(&theme)
            .with_prompt("Choose an option")
            .items(&options)
            .default(0)
            .interact_opt()?;
        utils::clear();

        if over_ride.is_none() {
            return Err(anyhow::anyhow!("No selection was made"));
        } else if over_ride == Some(0) {
            let options = vec!["Update existing override", "Add new override"];
            let theme = theme::CustomTheme {};
            let select = Select::with_theme(&theme)
                .with_prompt("Would you like to update an existing setting or add a new one?")
                .items(&options)
                .default(0)
                .interact_opt()?;
            utils::clear();
            if select.is_none() {
                return Err(anyhow::anyhow!("No selection was made"));
            } else if select == Some(0) {
                // Updating existing one
                skip_override::interactive_update_override(&client).await?;
            } else if select == Some(1) {
                // Adding new one
                let theme = theme::CustomTheme {};
                let input = Input::with_theme(&theme)
                    .with_prompt("Enter the name of the anime")
                    .interact_text()?;
                let anime_id = api::anilist::fetch::search(&client, input).await?;
                utils::clear();

                let options = vec!["Opening", "Credits", "Recap", "Filler"];
                let selection = MultiSelect::with_theme(&theme)
                    .with_prompt("What overrides do you want to enable?")
                    .items(&options)
                    .interact_opt()?;
                utils::clear();
                if selection.is_none() {
                    return Err(anyhow::anyhow!("No selection was made"));
                } else if selection.is_some() {
                    let selected = selection.unwrap();
                    let mut intro = false;
                    let mut outro = false;
                    let mut recap = false;
                    let mut filler = false;
                    for i in selected {
                        if i == 0 {
                            intro = true;
                        } else if i == 1 {
                            outro = true;
                        } else if i == 2 {
                            recap = true;
                        } else if i == 3 {
                            filler = true;
                        }
                    }
                    skip_override::add_override(anime_id, intro, outro, recap, filler);
                }
                println!("Override has been successfully saved!");
            }
        } else if over_ride == Some(1) {
            // Deleting existing one
            skip_override::interactive_delete_override(&client).await?;
        }
    }

    Ok(())
}

async fn sequel(client: &Client, anime_id: i32) -> Result<i32> {
    let sequel = api::anilist::fetch::sequel_data(&client, anime_id).await;
    match sequel {
        Ok(s) => {
            println!("Sequel: {}\n", s.title);
            let theme = theme::CustomTheme {};
            let options = vec![
                "Add sequel to \"Currently watching\"",
                "Don't add sequel to my watchlist",
            ];
            let select = Select::with_theme(&theme)
                .with_prompt("Would you like to add the sequel to watchlist?")
                .items(&options)
                .default(0)
                .interact_opt()
                .unwrap();

            utils::clear();
            match select {
                Some(id) => {
                    if id == 0 {
                        api::anilist::mutation::update_status(&client, s.id, 0).await?;

                        let options = vec!["Yes", "No"];
                        let select = Select::with_theme(&theme)
                            .with_prompt("Would you like to continue watching with the sequel?")
                            .items(&options)
                            .default(0)
                            .clear(true)
                            .interact_opt()
                            .unwrap();
                        match select {
                            Some(id) => {
                                if id == 1 {
                                    Err(anyhow::anyhow!(
                                        "User chose not to continue watching with the sequel."
                                    ))
                                } else {
                                    Ok(s.id)
                                }
                            }
                            None => Err(anyhow::anyhow!("No selection was made")),
                        }
                    } else {
                        println!("See you later!");
                        process::exit(0);
                    }
                }
                None => Err(anyhow::anyhow!("No selection made")),
            }
        }
        Err(e) => Err(anyhow::anyhow!("Failed to get sequel data: {}", e)),
    }
}

async fn watch(
    client: &Client,
    config: config::Config,
    mut rpc_client: discord_rpc_client::Client,
    anime_data: api::anilist::user_fetch::AnimeData,
    syncing: bool,
) -> Result<()> {
    let mut cur_ep = anime_data.progress;
    let mut anime_id = anime_data.id;
    let mut max_ep = anime_data.episodes;
    let mut anime_name = anime_data.title;
    let mut mal_id = api::anilist::fetch::id_converter(&client, anime_id).await?;

    let mut cache: HashMap<u32, String> = default::Default::default();

    // Start initial player
    player::start_watching(&client, anime_id, mal_id, cur_ep, &config, &anime_name).await?;

    // Main watching loop
    loop {
        let binge = player::watching(
            &client,
            anime_id,
            mal_id,
            cur_ep + 1,
            max_ep,
            &config,
            &anime_name,
            syncing,
            &mut rpc_client,
            &mut cache,
        )
        .await?;

        log::info!("Binge watching: {}", binge);

        // If this was the last episode of the series
        if binge {
            cur_ep = cur_ep + 1;
            if cur_ep == max_ep {
                // Handle scoring if enabled
                if config.score_on_completion {
                    utils::clear();
                    let theme = theme::CustomTheme {};
                    let new_score: f64 = loop {
                        let input: String = Input::with_theme(&theme)
                            .with_prompt("Enter a score on a scale of 1 to 10")
                            .interact_text()
                            .expect("Failed to read input, or input was invalid");
                        match input.parse::<f64>() {
                            Ok(score) if score >= 1.0 && score <= 10.0 => break score,
                            _ => {
                                println!("Score must be between 1 and 10.");
                                continue;
                            }
                        }
                    };
                    utils::clear();
                    api::anilist::mutation::update_score(&client, anime_id, new_score).await?;
                }

                println!("This was the last episode of the season.");

                // Check for sequel
                let sequel = sequel(&client, anime_id).await;
                if let Ok(sequel_id) = sequel {
                    // Update status for the sequel
                    if config.score_on_completion {
                        api::anilist::mutation::update_status(&client, sequel_id, 0).await?;
                    } else {
                        api::anilist::mutation::update_progress(&client, sequel_id, 0).await?;
                    }

                    // Update anime information for the sequel
                    anime_id = sequel_id;
                    cur_ep = 0;
                    let data = api::anilist::fetch::data_by_id(&client, anime_id).await?;
                    max_ep = data.episodes;
                    anime_name = data.title;

                    println!("Starting the sequel...");
                    mal_id = api::anilist::fetch::id_converter(&client, sequel_id).await?;

                    let client_clone = client.clone();
                    let config_lang = config.language.clone();
                    let config_quality = config.quality.clone();
                    let config_sub_or_dub = config.sub_or_dub.clone();
                    let name_clone = anime_name.clone();
                    let config_clone = config.clone();
                    let mut next_ep = cur_ep;
                    tokio::task::spawn(async move {
                        if skip_override::search(anime_id).filler {
                            if !config_clone.skip_filler {
                                next_ep = player::filler(&client_clone, mal_id, next_ep)
                                    .await
                                    .unwrap();
                            }
                        } else if config_clone.skip_filler {
                            next_ep = player::filler(&client_clone, mal_id, next_ep)
                                .await
                                .unwrap();
                        }

                        let mut next_url: Result<String> = Err(anyhow::anyhow!(""));
                        while next_url.is_err() {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            next_url = player::get_url(
                                &client_clone,
                                &config_lang,
                                mal_id,
                                anime_id,
                                next_ep,
                                &config_quality,
                                &config_sub_or_dub,
                                &name_clone,
                            )
                            .await
                            .with_context(|| {
                                format!("Failed to fetch URL for episode {}", next_ep)
                            });
                            if next_url.is_err() {
                                eprintln!("Failed to get episode link, retrying...");
                                log::warn!("Failed to get episode link for id: {}", anime_id);
                            }
                        }

                        mpvipc::send_command(&["loadfile", &next_url.unwrap()])
                            .await
                            .unwrap();
                    });

                    // Continue to the next iteration without exiting
                    continue;
                } else {
                    // No sequel, exit
                    break;
                }
            }

            let mut ep_to_get = cur_ep+1;
            let override_setting = skip_override::search(anime_id);
            if override_setting.filler {
                if !config.skip_filler {
                    ep_to_get = player::filler(&client, mal_id, ep_to_get).await?;
                }
            } else if config.skip_filler {
                ep_to_get = player::filler(&client, mal_id, ep_to_get).await?;
            }

            let next_url = cache.get(&ep_to_get);

            match next_url {
                Some(url) => {
                    mpvipc::send_command(&["loadfile", &url]).await.unwrap();
                    log::debug!("Episode loaded");
                }
                None => {
                    println!("No link prefetched, fetching now.");
                    let override_setting = skip_override::search(anime_id);

                    if override_setting.filler {
                        if !config.skip_filler {
                            ep_to_get = player::filler(&client, mal_id, cur_ep).await?;
                        }
                    } else if config.skip_filler {
                        cur_ep = player::filler(&client, mal_id, cur_ep).await?;
                    }

                    let mut url = player::get_url(
                        &client,
                        &config.language,
                        mal_id,
                        anime_id,
                        ep_to_get,
                        &config.quality,
                        &config.sub_or_dub,
                        &anime_name,
                    )
                    .await;

                    while url.is_err() {
                        sleep(Duration::from_secs(3)).await;
                        url = player::get_url(
                            &client,
                            &config.language,
                            mal_id,
                            anime_id,
                            cur_ep+1,
                            &config.quality,
                            &config.sub_or_dub,
                            &anime_name,
                        )
                        .await;
                    }

                    mpvipc::send_command(&["loadfile", &url.unwrap()]).await.unwrap();
                    log::debug!("Episode loaded");
                }
            }
        } else {
            // Not binging, exit
            log::info!("Not binging");
            break;
        }
    }

    utils::clear();
    println!("See you later!");
    if config.discord_presence {
        rpc_client.clear_activity().unwrap();
    }

    Ok(())
}

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    process::{Command, Stdio},
};

use anyhow::Context;
use clap::Parser;
use inquire::{MultiSelect, Select, Text};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
struct Cli {
    /// The path to the config file
    #[clap(long, default_value = "config.json")]
    config: String,

    #[clap(subcommand)]
    subcommand: SubCommand,
}

#[derive(Parser)]
enum SubCommand {
    /// Add new images and tags to the config
    Add(AddCommand),
    /// Pull images from the config
    Pull,
    /// Clean images listed in the config
    Clean,
    /// Edit images and tags in the config
    Edit,
    /// Push images to a registry
    Push(PushCommand),
}

#[derive(Parser)]
struct AddCommand {
    /// The library to pull the images from
    #[clap(short, long)]
    library: Option<String>,

    /// The name of the image to pull
    #[clap(short, long)]
    name: Option<String>,

    /// The tags to pull
    #[clap(short, long)]
    tags: Option<Vec<String>>,
}

#[derive(Parser)]
struct PushCommand {
    /// The registry to push the images to
    #[clap(long)]
    repo: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();

    let mut config = read_config(&cli.config)?;

    match cli.subcommand {
        SubCommand::Add(command) => add(&mut config, &command).await?,
        SubCommand::Pull => pull(&config).await?,
        SubCommand::Clean => clean(&config).await?,
        SubCommand::Edit => edit(&mut config).await?,
        SubCommand::Push(command) => push(&config, &command).await?,
    }

    write_config(&cli.config, &config)?;

    Ok(())
}

fn read_config(path: &str) -> anyhow::Result<Config> {
    let path = std::path::Path::new(path);
    if !path.exists() {
        return Ok(Default::default());
    }

    let content = std::fs::read_to_string(path).context("Failed to read config")?;
    serde_json::from_str(&content).context("Failed to parse config")
}

fn write_config(path: &str, config: &Config) -> anyhow::Result<()> {
    let content = serde_json::to_string_pretty(config).context("Failed to serialize config")?;
    std::fs::write(path, content).context("Failed to write config")
}

async fn push(config: &Config, command: &PushCommand) -> anyhow::Result<()> {
    let mut responses: HashMap<String, FetchTagsResponse> = HashMap::new();

    for profile in config.pull_profiles.values() {
        for tag in &profile.tags {
            let library = profile.library.as_ref().map(|l| l.as_str());

            let response_key = image_name(library, &profile.repo);

            let response = if let Some(response) = responses.get(&response_key) {
                response.clone()
            } else {
                let response = fetch_tags(library, &profile.repo)
                    .await
                    .context("Failed to fetch tags")?;
                responses.insert(response_key.clone(), response.clone());
                response
            };

            let image = image_name(library, &profile.repo);
            let mut targets = vec![format!("{}/{}:{}", command.repo, &image, tag)];

            let item = response.results.iter().find(|item| &item.name == tag);
            if let Some(item) = item {
                targets.extend(
                    response
                        .results
                        .iter()
                        .filter(|x| x.digest == item.digest)
                        .map(|item| format!("{}/{}:{}", command.repo, &image, item.name)),
                );
            }

            for target in targets {
                docker_command()
                    .arg("tag")
                    .arg(format!("{}:{}", &image, tag))
                    .arg(&target)
                    .output()
                    .context("Failed to tag image")?;

                docker_command()
                    .arg("push")
                    .arg(&target)
                    .output()
                    .context("Failed to push image")?;

                docker_command()
                    .arg("image")
                    .arg("rm")
                    .arg(&target)
                    .output()
                    .context("Failed to remove image")?;
            }
        }
    }

    Ok(())
}

async fn edit(config: &mut Config) -> anyhow::Result<()> {
    let profiles = config
        .pull_profiles
        .keys()
        .map(|key| key.clone())
        .collect::<Vec<_>>();

    let profile = Select::new("Please choose profile to edit:", profiles)
        .prompt()
        .context("Failed to prompt")?;

    let profile = config
        .pull_profiles
        .get_mut(&profile)
        .expect("Profile not found");

    let tags = profile
        .tags
        .iter()
        .map(|tag| tag.clone())
        .collect::<Vec<_>>();

    let tags = MultiSelect::new("Please choose tags to keep:", tags)
        .with_all_selected_by_default()
        .prompt()
        .context("Failed to prompt")?;

    profile.tags = tags.into_iter().collect();

    Ok(())
}

async fn add(config: &mut Config, command: &AddCommand) -> anyhow::Result<()> {
    let library = if let Some(library) = &command.library {
        library.clone()
    } else {
        Text::new("Library:")
            .with_help_message("empty for _")
            .prompt()
            .context("Failed to prompt")?
            .trim()
            .to_string()
    };

    let library = if library.is_empty() {
        None
    } else {
        Some(library.as_ref())
    };

    let name = if let Some(name) = &command.name {
        name.clone()
    } else {
        Text::new("Name:").prompt().context("Failed to prompt")?
    };

    let names = if let Some(tags) = &command.tags {
        tags.clone()
    } else {
        let mut response = fetch_tags(library, &name).await?;
        response.results.sort_by(|a, b| b.name.cmp(&a.name));

        let names = response
            .results
            .iter()
            .map(|item| item.name.clone())
            .collect::<Vec<_>>();

        MultiSelect::new("Please choose wanted images:", names)
            .prompt()
            .context("Failed to prompt")?
    };

    let image = image_name(library, &name);

    let profile = config
        .pull_profiles
        .entry(image)
        .or_insert_with(|| PullProfile {
            library: library.map(|l| l.to_string()),
            repo: name.clone(),
            tags: Default::default(),
        });

    profile.tags.extend(names);

    Ok(())
}

async fn clean(config: &Config) -> anyhow::Result<()> {
    for profile in config.pull_profiles.values() {
        for tag in &profile.tags {
            docker_command()
                .arg("image")
                .arg("rm")
                .arg(&format!("{}:{}", profile.repo, tag))
                .status()
                .context("Failed to remove image")?;
        }
    }

    Ok(())
}

fn docker_command() -> Command {
    let mut command = Command::new("docker");
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());

    command
}

async fn pull(config: &Config) -> anyhow::Result<()> {
    for profile in config.pull_profiles.values() {
        for tag in &profile.tags {
            docker_command()
                .arg("pull")
                .arg(&format!("{}:{}", profile.repo, tag))
                .status()
                .context("Failed to pull image")?;
        }
    }

    Ok(())
}

fn image_name(library: Option<&str>, repo: &str) -> String {
    if let Some(library) = library {
        format!("{}/{}", library, repo)
    } else {
        repo.to_string()
    }
}

async fn fetch_tags(library: Option<&str>, repo: &str) -> anyhow::Result<FetchTagsResponse> {
    let url = format!(
        "https://hub.docker.com/v2/repositories/{}/{}/tags?page_size=100&ordering=last_updated",
        library.unwrap_or("library"),
        repo
    );

    let image = image_name(library, repo);

    reqwest::get(url)
        .await
        .with_context(|| format!("Failed to fetch tags for {image}"))?
        .json()
        .await
        .with_context(|| format!("Failed to parse response for {image}"))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Config {
    pull_profiles: BTreeMap<String, PullProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PullProfile {
    library: Option<String>,
    repo: String,
    tags: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FetchTagsResponse {
    results: Vec<FetchTagsItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FetchTagsItem {
    name: String,
    digest: String,
}

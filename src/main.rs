use std::io::{Write, IsTerminal};
use std::process::{Command, Stdio};

use aws_sdk_ec2::types::{Filter, Instance, Tag};
use aws_sdk_ec2::Client;
use clap::{Parser, Subcommand};
use tabled::{Table, Tabled};

use aws_sdk_ec2::client::Waiters;
use aws_smithy_async::rt::sleep::default_async_sleep;

#[derive(Parser)]
#[command(name = "ec2", about = "EC2 instance management CLI", version)]
struct Cli {
    /// Use less pager for output (default: auto when stdout is a terminal)
    #[arg(long)]
    pager: Option<bool>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Parser, Clone)]
struct ProfileArgs {
    /// AWS profile to use (default: ml-dev)
    #[arg(short, long, default_value = "ml-dev")]
    profile: String,
}

#[derive(Subcommand)]
enum Commands {
    /// List all EC2 instances
    List {
        #[command(flatten)]
        profile: ProfileArgs,

        /// Filter by instance state (comma-separated, e.g. running,stopped)
        #[arg(short, long)]
        state: Option<String>,
    },

    /// Start one or more EC2 instances
    Start {
        #[command(flatten)]
        profile: ProfileArgs,

        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Stop one or more EC2 instances
    Stop {
        #[command(flatten)]
        profile: ProfileArgs,

        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Restart (stop+start) one or more EC2 instances
    Restart {
        #[command(flatten)]
        profile: ProfileArgs,

        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Wait for instances to stop
    Wait {
        #[command(flatten)]
        profile: ProfileArgs,

        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Change instance type
    ChangeType {
        #[command(flatten)]
        profile: ProfileArgs,

        /// New instance type (e.g. t3.large)
        #[arg(value_name = "INSTANCE_TYPE")]
        instance_type: String,

        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Terminate an instance (requires confirmation)
    Terminate {
        #[command(flatten)]
        profile: ProfileArgs,

        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },
}

#[derive(Tabled)]
struct InstanceInfo {
    #[tabled(rename = "ID")]
    instance_id: String,

    #[tabled(rename = "Name")]
    name: String,

    #[tabled(rename = "Type")]
    instance_type: String,

    #[tabled(rename = "State")]
    state: String,

    #[tabled(rename = "Private IP")]
    private_ip: String,
}

fn set_profile(profile: &str) {
    std::env::set_var("AWS_PROFILE", profile);
}

async fn get_client(profile: &str) -> Client {
    set_profile(profile);
    let config = aws_config::load_from_env().await;

    // Set the async sleep implementation required by the retry system
    let config = config
        .to_builder()
        .sleep_impl(default_async_sleep().unwrap())
        .build();

    Client::new(&config)
}

fn extract_name(tags: &[Tag]) -> String {
    tags.iter()
        .find(|t| t.key() == Some("Name"))
        .and_then(|t| t.value().map(|v| v.to_string()))
        .unwrap_or_else(|| "-".to_string())
}

fn extract_instance_type(instance: &Instance) -> String {
    instance.instance_type().map(|t| t.as_str()).unwrap_or("-").to_string()
}

fn extract_state(instance: &Instance) -> String {
    instance
        .state()
        .and_then(|s| s.name())
        .map(|n| n.as_str())
        .unwrap_or("-")
        .to_string()
}

async fn describe_instances(client: &Client, states: &[String]) -> Vec<InstanceInfo> {
    let mut filter_builder = Filter::builder().name("instance-state-name");
    if states.is_empty() {
        for s in ["pending", "running", "stopping", "stopped"] {
            filter_builder = filter_builder.values(s.to_string());
        }
    } else {
        for s in states {
            filter_builder = filter_builder.values(s.clone());
        }
    }
    let filter = filter_builder.build();

    let resp = client
        .describe_instances()
        .set_filters(Some(vec![filter]))
        .send()
        .await
        .unwrap();

    let mut instances = Vec::new();
    for reservation in resp.reservations() {
        for instance in reservation.instances() {
            let name = extract_name(instance.tags());
            instances.push(InstanceInfo {
                instance_id: instance.instance_id().unwrap_or("-").to_string(),
                name,
                instance_type: extract_instance_type(instance),
                state: extract_state(instance),
                private_ip: instance.private_ip_address().unwrap_or("-").to_string(),
            });
        }
    }
    instances
}

async fn interactive_select(instances: &[InstanceInfo]) -> Option<usize> {
    print!("Enter instance number (or 0 to cancel): ");
    std::io::stdout().flush().unwrap();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
    let num: usize = input.trim().parse().unwrap_or(0);
    if num > 0 && num <= instances.len() {
        Some(num - 1)
    } else {
        None
    }
}

fn should_use_pager(cli: &Cli) -> bool {
    match cli.pager {
        Some(v) => v,
        None => std::io::stdout().is_terminal(),
    }
}

fn run_with_pager(output: Vec<u8>) {
    let mut child = Command::new("less")
        .arg("-R") // pass through ANSI color codes
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .spawn()
        .expect("Failed to spawn less");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&output).expect("Failed to write to pager");
    }

    let status = child.wait().expect("Failed to wait for pager");
    if !status.success() {
        eprintln!("pager exited with status: {}", status);
    }
}

async fn cmd_list(client: &Client, states: Option<String>) -> Vec<u8> {
    let states_vec: Vec<String> = states
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default();

    let instances = describe_instances(client, &states_vec).await;

    if instances.is_empty() {
        let msg = format!("\x1b[31mNo instances found.\x1b[0m\n");
        return msg.into_bytes();
    }

    let table = Table::new(&instances);
    let output = format!("\n{}", table.to_string());
    output.into_bytes()
}

async fn cmd_start(client: &Client, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    client
        .start_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    let mut output = Vec::new();
    output.extend_from_slice(b"\x1b[32mWaiting for instances to start...\x1b[0m\n");

    client
        .wait_until_instance_running()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    output.extend_from_slice(&format!("\x1b[32mInstance(s) {} are running\x1b[0m\n", ids.join(", ")).into_bytes());
    output
}

async fn cmd_stop(client: &Client, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    client
        .stop_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    format!("\x1b[33mInstance(s) {} stopped\x1b[0m\n", ids.join(", ")).into_bytes()
}

async fn cmd_restart(client: &Client, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::new();
    output.extend_from_slice(b"\x1b[33mStopping instances...\x1b[0m\n");

    client
        .stop_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    client
        .wait_until_instance_stopped()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    output.extend_from_slice(b"\x1b[33mStarting instances...\x1b[0m\n");

    client
        .start_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    client
        .wait_until_instance_running()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    output.extend_from_slice(&format!("\x1b[32mInstance(s) {} restarted successfully\x1b[0m\n", ids.join(", ")).into_bytes());
    output
}

async fn cmd_wait(client: &Client, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::new();
    output.extend_from_slice(b"\x1b[33mWaiting for instances to stop...\x1b[0m\n");

    client
        .wait_until_instance_stopped()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    output.extend_from_slice(&format!("\x1b[32mInstance(s) {} are stopped\x1b[0m\n", ids.join(", ")).into_bytes());
    output
}

async fn cmd_change_type(client: &Client, instance_type: String, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::new();
    for id in &ids {
        output.extend_from_slice(&format!("\x1b[33mChanging {} to {}...\x1b[0m\n", id, instance_type).into_bytes());

        client
            .modify_instance_attribute()
            .instance_id(id)
            .instance_type(aws_sdk_ec2::types::AttributeValue::builder()
                .value(&instance_type)
                .build())
            .send()
            .await
            .unwrap();

        output.extend_from_slice(&format!("\x1b[32mInstance {} type changed\x1b[0m\n", id).into_bytes());
    }
    output
}

async fn cmd_terminate(client: &Client, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    let instances = describe_instances_by_ids(client, &ids).await;
    let table = Table::new(&instances);
    let mut output = format!("\n{}", table.to_string()).into_bytes();

    output.extend_from_slice(b"\x1b[31mWARNING: Termination permanently deletes the instance!\x1b[0m\n");
    output.extend_from_slice(b"Type \"terminate\" to confirm: ");
    std::io::stdout().flush().unwrap();

    let mut confirmation = String::new();
    std::io::stdin().read_line(&mut confirmation).unwrap();

    if confirmation.trim() != "terminate" {
        output.clear();
        output.extend_from_slice(b"\x1b[33mTermination cancelled.\x1b[0m\n");
        return output;
    }

    client
        .terminate_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    output.extend_from_slice(&format!("\x1b[32mInstance(s) {} terminated\x1b[0m\n", ids.join(", ")).into_bytes());
    output
}

async fn resolve_instance_ids(client: &Client, provided_ids: &[String]) -> Vec<String> {
    if !provided_ids.is_empty() {
        return provided_ids.to_vec();
    }

    let instances = describe_instances(client, &[]).await;

    if instances.is_empty() {
        return Vec::new();
    }

    println!("\nAvailable instances:");
    for (i, inst) in instances.iter().enumerate() {
        println!(
            "  {}. {} | {} | {} | {}",
            i + 1,
            inst.instance_id,
            inst.name,
            inst.instance_type,
            inst.state
        );
    }

    match interactive_select(&instances).await {
        Some(idx) => vec![instances[idx].instance_id.clone()],
        None => Vec::new(),
    }
}

async fn describe_instances_by_ids(client: &Client, instance_ids: &[String]) -> Vec<InstanceInfo> {
    let resp = client
        .describe_instances()
        .set_instance_ids(Some(instance_ids.to_vec()))
        .send()
        .await
        .unwrap();

    let mut instances = Vec::new();
    for reservation in resp.reservations() {
        for instance in reservation.instances() {
            let name = extract_name(instance.tags());
            instances.push(InstanceInfo {
                instance_id: instance.instance_id().unwrap_or("-").to_string(),
                name,
                instance_type: extract_instance_type(instance),
                state: extract_state(instance),
                private_ip: instance.private_ip_address().unwrap_or("-").to_string(),
            });
        }
    }
    instances
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let profile = match &cli.command {
        Commands::List { profile, .. } => &profile.profile,
        Commands::Start { profile, .. } => &profile.profile,
        Commands::Stop { profile, .. } => &profile.profile,
        Commands::Restart { profile, .. } => &profile.profile,
        Commands::Wait { profile, .. } => &profile.profile,
        Commands::ChangeType { profile, .. } => &profile.profile,
        Commands::Terminate { profile, .. } => &profile.profile,
    };

    let client = get_client(profile).await;

    let use_pager = should_use_pager(&cli);

    match cli.command {
        Commands::List { state, .. } => {
            let output = cmd_list(&client, state).await;
            if use_pager {
                run_with_pager(output);
            } else {
                print!("{}", String::from_utf8_lossy(&output));
            }
        }
        Commands::Start { instance_ids, .. } => {
            let output = cmd_start(&client, instance_ids).await;
            print!("{}", String::from_utf8_lossy(&output));
        }
        Commands::Stop { instance_ids, .. } => {
            let output = cmd_stop(&client, instance_ids).await;
            print!("{}", String::from_utf8_lossy(&output));
        }
        Commands::Restart { instance_ids, .. } => {
            let output = cmd_restart(&client, instance_ids).await;
            print!("{}", String::from_utf8_lossy(&output));
        }
        Commands::Wait { instance_ids, .. } => {
            let output = cmd_wait(&client, instance_ids).await;
            print!("{}", String::from_utf8_lossy(&output));
        }
        Commands::ChangeType {
            instance_type,
            instance_ids,
            ..
        } => {
            let output = cmd_change_type(&client, instance_type, instance_ids).await;
            print!("{}", String::from_utf8_lossy(&output));
        }
        Commands::Terminate { instance_ids, .. } => {
            let output = cmd_terminate(&client, instance_ids).await;
            if use_pager {
                run_with_pager(output);
            } else {
                print!("{}", String::from_utf8_lossy(&output));
            }
        }
    }
}

use std::io::Write;

use aws_sdk_ec2::types::{Filter, Instance, Tag};
use aws_sdk_ec2::Client;
use clap::{Parser, Subcommand};
use tabled::{Table, Tabled};

use aws_sdk_ec2::client::Waiters;
use aws_smithy_async::rt::sleep::default_async_sleep;

// ANSI color codes
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

// State colors
const GREEN: &str = "\x1b[32m";      // Success/running
const YELLOW: &str = "\x1b[33m";     // Warning/stopping/waiting
const RED: &str = "\x1b[31m";        // Error/stopped/terminate
const CYAN: &str = "\x1b[36m";       // Info
const BLUE: &str = "\x1b[34m";       // Neutral

// State-specific colors
const STATE_RUNNING: &str = "\x1b[32m";    // Bright green
const STATE_STOPPED: &str = "\x1b[33m";    // Yellow
const STATE_STOPPING: &str = "\x1b[33m";   // Yellow (dim)
const STATE_PENDING: &str = "\x1b[36m";    // Cyan
const STATE_SHUTTING_DOWN: &str = "\x1b[31m"; // Red
const STATE_TERMINATED: &str = "\x1b[90m";  // Gray

fn state_color(state: &str) -> &'static str {
    match state.to_lowercase().as_str() {
        "running" => STATE_RUNNING,
        "pending" => STATE_PENDING,
        "stopping" => STATE_STOPPING,
        "stopped" => STATE_STOPPED,
        "shutting-down" => STATE_SHUTTING_DOWN,
        "terminated" => STATE_TERMINATED,
        _ => RESET,
    }
}

fn state_emoji(state: &str) -> &str {
    match state.to_lowercase().as_str() {
        "running" => "🟢",
        "pending" => "🔵",
        "stopping" => "🟡",
        "stopped" => "⭕",
        "shutting-down" => "🔴",
        "terminated" => "⚫",
        _ => "⚪",
    }
}

#[derive(Parser)]
#[command(name = "ec2", about = "EC2 instance management CLI", version)]
struct Cli {
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
    
    let states_to_filter: Vec<String> = if states.is_empty() {
        ["pending", "running", "stopping", "stopped"].iter().map(|s| s.to_string()).collect()
    } else {
        states.to_vec()
    };
    
    for s in states_to_filter {
        filter_builder = filter_builder.values(s);
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



async fn cmd_list(client: &Client, states: Option<String>) -> Vec<u8> {
    let states_vec: Vec<String> = states
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default();

    let instances = describe_instances(client, &states_vec).await;

    if instances.is_empty() {
        let msg = format!("{}🔍 No instances found.{}\n", RED, RESET);
        return msg.into_bytes();
    }

    // Build a colorized table output
    let mut output = Vec::new();
    output.extend_from_slice(&format!("\n{}{}{} ({} instance{}){}\n", BOLD, CYAN, "EC2 Instances", instances.len(), if instances.len() == 1 {""} else {"s"}, RESET).into_bytes());
    
    // Table header
    output.extend_from_slice(&format!(
        "  {} | {} | {} | {} | {}\n",
        format!("{}ID{}", BOLD, RESET),
        format!("{}Name{}", BOLD, RESET),
        format!("{}Type{}", BOLD, RESET),
        format!("{}State{}", BOLD, RESET),
        format!("{}IP{}", BOLD, RESET)
    ).into_bytes());
    
    // Separator
    output.extend_from_slice(b"  ---|------|------|------|------\n");
    
    // Table rows
    for inst in &instances {
        let state_colored = format!("{}{}{}", state_color(&inst.state), inst.state, RESET);
        let name_colored = if inst.name != "-" {
            format!("{}{}{}", BOLD, inst.name, RESET)
        } else {
            format!("{}{}{}", DIM, inst.name, RESET)
        };
        
        output.extend_from_slice(&format!(
            "  {} | {} {} | {} | {} | {}\n",
            state_emoji(&inst.state),
            inst.instance_id,
            name_colored,
            inst.instance_type,
            state_colored,
            inst.private_ip
        ).into_bytes());
    }
    
    output
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
    output.extend_from_slice(&format!("{}⏳ Waiting for instances to start...{}\n", YELLOW, RESET).into_bytes());

    client
        .wait_until_instance_running()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    output.extend_from_slice(&format!("{}✅ Instance(s) {} are now running{}\n", GREEN, ids.join(", "), RESET).into_bytes());
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

    format!("{}⏹️ Instance(s) {} stopped{}\n", YELLOW, ids.join(", "), RESET).into_bytes()
}

async fn cmd_restart(client: &Client, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::new();
    output.extend_from_slice(&format!("{}⏹️ Stopping instances...{}\n", YELLOW, RESET).into_bytes());

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

    output.extend_from_slice(&format!("{}▶️ Starting instances...{}\n", YELLOW, RESET).into_bytes());

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

    output.extend_from_slice(&format!("{}✅ Instance(s) {} restarted successfully{}\n", GREEN, ids.join(", "), RESET).into_bytes());
    output
}

async fn cmd_wait(client: &Client, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::new();
    output.extend_from_slice(&format!("{}⏳ Waiting for instances to stop...{}\n", YELLOW, RESET).into_bytes());

    client
        .wait_until_instance_stopped()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    output.extend_from_slice(&format!("{}✅ Instance(s) {} are now stopped{}\n", GREEN, ids.join(", "), RESET).into_bytes());
    output
}

async fn cmd_change_type(client: &Client, instance_type: String, instance_ids: Vec<String>) -> Vec<u8> {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::new();
    for id in &ids {
        output.extend_from_slice(&format!("{}🔄 Changing {} to {}...{}\n", YELLOW, id, instance_type, RESET).into_bytes());

        client
            .modify_instance_attribute()
            .instance_id(id)
            .instance_type(aws_sdk_ec2::types::AttributeValue::builder()
                .value(&instance_type)
                .build())
            .send()
            .await
            .unwrap();

        output.extend_from_slice(&format!("{}✅ Instance {} type changed to {}{}\n", GREEN, id, instance_type, RESET).into_bytes());
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
    let mut output = format!("\n{}🗑️ Instances to terminate:{}\n{}", RED, RESET, table.to_string()).into_bytes();

    output.extend_from_slice(&format!("\n{}⚠️  WARNING: Termination permanently deletes the instance!{}\n", RED, RESET).into_bytes());
    output.extend_from_slice(&format!("{}Type \"terminate\" to confirm:{}\n", YELLOW, RESET).into_bytes());
    std::io::stdout().flush().unwrap();

    let mut confirmation = String::new();
    std::io::stdin().read_line(&mut confirmation).unwrap();

    if confirmation.trim() != "terminate" {
        output.clear();
        output.extend_from_slice(&format!("{}❌ Termination cancelled.{}\n", YELLOW, RESET).into_bytes());
        return output;
    }

    client
        .terminate_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    output.clear();
    output.extend_from_slice(&format!("{}✅ Instance(s) {} terminated successfully{}\n", GREEN, ids.join(", "), RESET).into_bytes());
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

    println!("\n{}🔍 Available instances:{}", CYAN, RESET);
    for (i, inst) in instances.iter().enumerate() {
        let state_colored = format!("{}{}{}", state_color(&inst.state), inst.state, RESET);
        println!(
            "  {}. {} | {} | {} | {}",
            i + 1,
            inst.instance_id,
            inst.name,
            inst.instance_type,
            state_colored
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

    match cli.command {
        Commands::List { state, .. } => {
            let output = cmd_list(&client, state).await;
            print!("{}", String::from_utf8_lossy(&output));
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
            print!("{}", String::from_utf8_lossy(&output));
        }
    }
}

use std::io::Write;

use aws_sdk_ec2::types::{Filter, Instance, Tag};
use aws_sdk_ec2::Client;
use clap::{Parser, Subcommand};
use tabled::{Table, Tabled};

use aws_sdk_ec2::client::Waiters;

#[derive(Parser)]
#[command(name = "ec2", about = "EC2 instance management CLI", version)]
struct Cli {
    /// AWS profile to use (default: ml-dev)
    #[arg(short, long, default_value = "ml-dev")]
    profile: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all EC2 instances
    List {
        /// Filter by instance state (comma-separated, e.g. running,stopped)
        #[arg(short, long)]
        state: Option<String>,
    },

    /// Start one or more EC2 instances
    Start {
        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Stop one or more EC2 instances
    Stop {
        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Restart (stop+start) one or more EC2 instances
    Restart {
        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Wait for instances to stop
    Wait {
        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Change instance type
    ChangeType {
        /// New instance type (e.g. t3.large)
        #[arg(value_name = "INSTANCE_TYPE")]
        instance_type: String,

        /// Instance IDs (if omitted, interactive selection)
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },

    /// Terminate an instance (requires confirmation)
    Terminate {
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
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
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

async fn cmd_list(client: &Client, states: Option<String>) {
    let states_vec: Vec<String> = states
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default();

    let instances = describe_instances(client, &states_vec).await;

    if instances.is_empty() {
        color_print::cprintln!("<red>No instances found.</red>");
        return;
    }

    let table = Table::new(&instances);
    println!("\n{}", table.to_string());
}

async fn cmd_start(client: &Client, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    client
        .start_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    color_print::cprintln!("<green>Waiting for instances to start...</green>");

    client
        .wait_until_instance_running()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    color_print::cprintln!("<green>Instance(s) {} are running</green>", ids.join(", "));
}

async fn cmd_stop(client: &Client, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    client
        .stop_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    color_print::cprintln!("<yellow>Instance(s) {} stopped</yellow>", ids.join(", "));
}

async fn cmd_restart(client: &Client, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    color_print::cprintln!("<yellow>Stopping instances...</yellow>");

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

    color_print::cprintln!("<yellow>Starting instances...</yellow>");

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

    color_print::cprintln!("<green>Instance(s) {} restarted successfully</green>", ids.join(", "));
}

async fn cmd_wait(client: &Client, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    color_print::cprintln!("<yellow>Waiting for instances to stop...</yellow>");

    client
        .wait_until_instance_stopped()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    color_print::cprintln!("<green>Instance(s) {} are stopped</green>", ids.join(", "));
}

async fn cmd_change_type(client: &Client, instance_type: String, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    for id in &ids {
        color_print::cprintln!("<yellow>Changing {} to {}...</yellow>", id, instance_type);

        client
            .modify_instance_attribute()
            .instance_id(id)
            .instance_type(aws_sdk_ec2::types::AttributeValue::builder()
                .value(&instance_type)
                .build())
            .send()
            .await
            .unwrap();

        color_print::cprintln!("<green>Instance {} type changed</green>", id);
    }
}

async fn cmd_terminate(client: &Client, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    let instances = describe_instances_by_ids(client, &ids).await;
    let table = Table::new(&instances);
    println!("\n{}", table.to_string());

    color_print::cprintln!("<red>WARNING: Termination permanently deletes the instance!</red>");
    print!("Type \"terminate\" to confirm: ");
    std::io::stdout().flush().unwrap();

    let mut confirmation = String::new();
    std::io::stdin().read_line(&mut confirmation).unwrap();

    if confirmation.trim() != "terminate" {
        color_print::cprintln!("<yellow>Termination cancelled.</yellow>");
        return;
    }

    client
        .terminate_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    color_print::cprintln!("<green>Instance(s) {} terminated</green>", ids.join(", "));
}

async fn resolve_instance_ids(client: &Client, provided_ids: &[String]) -> Vec<String> {
    if !provided_ids.is_empty() {
        return provided_ids.to_vec();
    }

    let instances = describe_instances(client, &[]).await;

    if instances.is_empty() {
        color_print::cprintln!("<red>No selectable EC2 instances found.</red>");
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
        None => {
            color_print::cprintln!("<yellow>No instance selected.</yellow>");
            Vec::new()
        }
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

    let client = get_client(&cli.profile).await;

    match cli.command {
        Commands::List { state } => cmd_list(&client, state).await,
        Commands::Start { instance_ids } => cmd_start(&client, instance_ids).await,
        Commands::Stop { instance_ids } => cmd_stop(&client, instance_ids).await,
        Commands::Restart { instance_ids } => cmd_restart(&client, instance_ids).await,
        Commands::Wait { instance_ids } => cmd_wait(&client, instance_ids).await,
        Commands::ChangeType {
            instance_type,
            instance_ids,
        } => cmd_change_type(&client, instance_type, instance_ids).await,
        Commands::Terminate { instance_ids } => cmd_terminate(&client, instance_ids).await,
    }
}

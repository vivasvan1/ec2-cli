use std::io::Write;
use std::process::Command;

use aws_sdk_ec2::types::{Filter, Instance, Tag};
use aws_sdk_ec2::Client;
use clap::{Parser, Subcommand};
use comfy_table::{Table, Cell, CellAlignment, ContentArrangement, TableStyle, LineStyle, ContentLineStyle};
use serde::Serialize;

use aws_sdk_ec2::client::Waiters;
use aws_smithy_async::rt::sleep::default_async_sleep;

// ─── Output format enum ────────────────────────────────────────────────
#[derive(clap::ValueEnum, Clone, Default, Debug)]
enum OutputFormat {
    /// Pretty table (default)
    #[default]
    Table,
    /// Pipe through less pager
    Less,
    /// JSON output
    Json,
}

// ─── State emoji mapping ───────────────────────────────────────────────
fn state_emoji(state: &str) -> &'static str {
    match state.to_lowercase().as_str() {
        "running" => "\u{1F7E2}",  // 🟢
        "pending" => "\u{1F535}",  // 🔵
        "stopping" => "\u{1F7E1}", // 🟡
        "stopped" => "\u{1F534}",  // 🔴
        "shutting-down" => "\u{1F7E0}", // 🟠
        "terminated" => "\u{1F534}", // ⚫
        _ => "\u{26AA}",           // ⚪
    }
}

// ─── CLI structs ───────────────────────────────────────────────────────
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

        /// Output format: table (pretty), less (paged), json
        #[arg(short, long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,
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

        /// Output format: table (pretty), less (paged), json
        #[arg(short, long, value_enum, default_value_t = OutputFormat::Table)]
        output: OutputFormat,
    },
}

// ─── Instance data ─────────────────────────────────────────────────────
#[derive(Debug, Serialize)]
struct InstanceInfo {
    instance_id: String,
    name: String,
    instance_type: String,
    state: String,
    private_ip: String,
}

// ─── AWS helpers ───────────────────────────────────────────────────────
fn set_profile(profile: &str) {
    std::env::set_var("AWS_PROFILE", profile);
}

async fn get_client(profile: &str) -> Client {
    set_profile(profile);
    let config = aws_config::load_from_env().await;

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
        ["pending", "running", "stopping", "stopped"]
            .iter()
            .map(|s| s.to_string())
            .collect()
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

async fn describe_instances_by_ids(client: &Client, ids: &[String]) -> Vec<InstanceInfo> {
    let resp = client
        .describe_instances()
        .set_instance_ids(Some(ids.to_vec()))
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

// ─── Beautiful table style (comfy-table v8 API) ──────────────────────
const BEAUTIFUL_STYLE: TableStyle = TableStyle::new()
    .top_border(LineStyle::new('┌', '─', '┬', '┐'))
    .header_lines(ContentLineStyle::new('│', '┆', '│'))
    .header_separator(LineStyle::new('╞', '═', '╪', '╡'))
    .content_lines(ContentLineStyle::new('│', '┆', '│'))
    .bottom_border(LineStyle::new('└', '─', '┴', '┘'));

// ─── Output helpers ────────────────────────────────────────────────────
fn render_instance_table(instances: &[InstanceInfo], title: &str) -> String {
    let mut table = Table::new();

    table.load_style(BEAUTIFUL_STYLE);

    let header = vec![
        Cell::new("  ID").set_alignment(CellAlignment::Left),
        Cell::new("  Name").set_alignment(CellAlignment::Left),
        Cell::new("  Type").set_alignment(CellAlignment::Left),
        Cell::new("  State").set_alignment(CellAlignment::Left),
        Cell::new("  Private IP").set_alignment(CellAlignment::Left),
    ];
    table.set_header(header);

    for inst in instances {
        let emoji = state_emoji(&inst.state);
        let state_display = format!("{} {}", emoji, inst.state.to_uppercase());
        let row = vec![
            Cell::new(&inst.instance_id).set_alignment(CellAlignment::Left),
            Cell::new(&inst.name).set_alignment(CellAlignment::Left),
            Cell::new(&inst.instance_type).set_alignment(CellAlignment::Left),
            Cell::new(state_display).set_alignment(CellAlignment::Left),
            Cell::new(&inst.private_ip).set_alignment(CellAlignment::Left),
        ];
        table.add_row(row);
    }

    table.set_content_arrangement(ContentArrangement::Dynamic);
    format!("\n  {}\n{}", title, table)
}

fn output_instances(instances: &[InstanceInfo], title: &str, format: &OutputFormat) {
    match format {
        OutputFormat::Table => {
            println!("{}", render_instance_table(instances, title));
        }
        OutputFormat::Less => {
            let content = render_instance_table(instances, title);
            pipe_to_less(&content);
        }
        OutputFormat::Json => {
            let json = serde_json::json!({
                "instances": instances,
                "count": instances.len(),
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
    }
}

fn pipe_to_less(content: &str) {
    let mut child = Command::new("less")
        .arg("-R")  // Allow ANSI color codes
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn less");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes()).unwrap();
    }

    child.wait().unwrap();
}

fn output_status(msg: impl AsRef<str>) {
    println!("  {}", msg.as_ref());
}

// ─── Command handlers ─────────────────────────────────────────────────
async fn cmd_list(client: &Client, states: Option<String>, format: &OutputFormat) {
    let states_vec: Vec<String> = states
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default();

    let instances = describe_instances(client, &states_vec).await;

    if instances.is_empty() {
        println!("\n  ⚠️  No instances found.\n");
        return;
    }

    let title = format!("EC2 Instances  ({} instance(s))", instances.len());
    output_instances(&instances, &title, format);
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

    output_status(format!("\u{1F680} Starting {} instance(s)...", ids.len()));

    client
        .wait_until_instance_running()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    output_status(format!("\u{2705} Instance(s) {} is now running", ids.join(", ")));
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

    output_status(format!("\u{1F6D1} Instance(s) {} stopped", ids.join(", ")));
}

async fn cmd_restart(client: &Client, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    output_status(format!("\u{1F504} Restarting {} instance(s)...", ids.len()));

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

    output_status("\u{23F3} Instances stopped, starting...");

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

    output_status(format!("\u{2705} Instance(s) {} restarted successfully", ids.join(", ")));
}

async fn cmd_wait(client: &Client, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    output_status(format!("\u{23F3} Waiting for {} instance(s) to stop...", ids.len()));

    client
        .wait_until_instance_stopped()
        .set_instance_ids(Some(ids.clone()))
        .wait(std::time::Duration::from_secs(300))
        .await
        .unwrap();

    output_status(format!("\u{2705} Instance(s) {} are now stopped", ids.join(", ")));
}

async fn cmd_change_type(client: &Client, instance_type: String, instance_ids: Vec<String>) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    output_status(format!("\u{1F527} Changing instance type to {}...", instance_type));

    for id in &ids {
        client
            .modify_instance_attribute()
            .instance_id(id)
            .instance_type(aws_sdk_ec2::types::AttributeValue::builder()
                .value(&instance_type)
                .build())
            .send()
            .await
            .unwrap();

        output_status(format!("     {} \u{2192} {}", id, instance_type));
    }

    output_status("\u{2705} Instance type(s) updated");
}

async fn cmd_terminate(client: &Client, instance_ids: Vec<String>, format: &OutputFormat) {
    let ids = resolve_instance_ids(client, &instance_ids).await;
    if ids.is_empty() {
        return;
    }

    let instances = describe_instances_by_ids(client, &ids).await;

    let title = "\u{1F4CB} Instances to terminate";
    output_instances(&instances, title, format);

    println!(
        "\n  \u{26A0}  WARNING: Termination permanently deletes the instance!"
    );
    print!("  Type \"terminate\" to confirm: ");
    std::io::stdout().flush().unwrap();

    let mut confirmation = String::new();
    std::io::stdin().read_line(&mut confirmation).unwrap();

    if confirmation.trim() != "terminate" {
        println!("  \u{274C} Termination cancelled.");
        return;
    }

    client
        .terminate_instances()
        .set_instance_ids(Some(ids.clone()))
        .send()
        .await
        .unwrap();

    println!("\n  \u{1F480} Instance(s) {} terminated successfully", ids.join(", "));
}

async fn resolve_instance_ids(client: &Client, provided_ids: &[String]) -> Vec<String> {
    if !provided_ids.is_empty() {
        return provided_ids.to_vec();
    }

    let instances = describe_instances(client, &[]).await;

    if instances.is_empty() {
        return Vec::new();
    }

    let title = "\u{1F4CB} Available instances";
    println!("\n{}", render_instance_table(&instances, title));

    print!("  Enter instance number (or 0 to cancel): ");
    std::io::stdout().flush().unwrap();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
    let num: usize = input.trim().parse().unwrap_or(0);
    if num > 0 && num <= instances.len() {
        vec![instances[num - 1].instance_id.clone()]
    } else {
        Vec::new()
    }
}

// ─── Main ──────────────────────────────────────────────────────────────
#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::List { profile, state, output } => {
            let client = get_client(&profile.profile).await;
            cmd_list(&client, state, &output).await;
        }
        Commands::Start { profile, instance_ids } => {
            let client = get_client(&profile.profile).await;
            cmd_start(&client, instance_ids).await;
        }
        Commands::Stop { profile, instance_ids } => {
            let client = get_client(&profile.profile).await;
            cmd_stop(&client, instance_ids).await;
        }
        Commands::Restart { profile, instance_ids } => {
            let client = get_client(&profile.profile).await;
            cmd_restart(&client, instance_ids).await;
        }
        Commands::Wait { profile, instance_ids } => {
            let client = get_client(&profile.profile).await;
            cmd_wait(&client, instance_ids).await;
        }
        Commands::ChangeType { profile, instance_type, instance_ids } => {
            let client = get_client(&profile.profile).await;
            cmd_change_type(&client, instance_type, instance_ids).await;
        }
        Commands::Terminate { profile, instance_ids, output } => {
            let client = get_client(&profile.profile).await;
            cmd_terminate(&client, instance_ids, &output).await;
        }
    }
}

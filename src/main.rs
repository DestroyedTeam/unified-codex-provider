mod auth;
mod config;
mod migrate;
mod oauth;
mod provider;
mod service;
mod sessions;
mod setup;
mod sync;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "ucp", version, about = "Unified Codex Provider manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show current provider status
    Status,
    /// List all registered providers
    List,
    /// Switch to a different provider
    Switch {
        /// Provider profile name
        name: String,
    },
    /// Remove a provider profile and its auth snapshot
    #[command(alias = "delete", alias = "rm")]
    Remove {
        /// Provider profile name
        name: String,
        /// Allow removing the active profile
        #[arg(long)]
        force: bool,
        /// Keep providers/{name}.auth.json
        #[arg(long)]
        keep_auth: bool,
    },
    /// Add a new provider profile interactively
    Add {
        /// Profile name
        name: String,
        #[arg(long)]
        model_provider: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        wire_api: Option<String>,
        #[arg(long)]
        context_window: Option<u64>,
        #[arg(long)]
        auto_compact_limit: Option<u64>,
    },
    /// Migrate existing config_*.toml / auth_*.json into profiles
    Init,
    /// Import auth from source auth_*.json files into profile snapshots
    ImportAuth {
        /// Profile name, or --all for all
        #[arg(long)]
        all: bool,
        /// Specific profile name
        name: Option<String>,
    },
    /// Login to OpenAI via browser OAuth and save as a new profile
    Login {
        /// Profile name for the new account
        #[arg(long)]
        name: Option<String>,
        /// Model to use for this OpenAI account profile
        #[arg(long)]
        model: Option<String>,
        /// Override the model_provider written to config.toml
        #[arg(long)]
        model_provider: Option<String>,
        /// Switch to this profile immediately after login
        #[arg(long)]
        switch: bool,
    },
    /// Run sync (usually triggered by LaunchAgent)
    Sync {
        #[arg(long)]
        auto: bool,
    },
    /// Run first-time setup checks and optional macOS auto-sync install
    Setup {
        /// Skip macOS LaunchAgent installation
        #[arg(long)]
        no_service: bool,
    },
    /// Diagnose the local UCP/Codex environment
    Doctor,
    /// Manage the macOS auto-sync LaunchAgent
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Generate shell completion script
    Completions {
        /// Shell to generate completions for
        shell: CompletionShell,
    },
    /// Internal dynamic completion helper
    #[command(name = "__complete", hide = true)]
    Complete {
        /// Completion candidate kind
        kind: CompleteKind,
        /// Optional prefix to filter candidates
        prefix: Option<String>,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Install or update the per-user LaunchAgent
    Install,
    /// Show LaunchAgent installation and runtime status
    Status,
    /// Unload and remove the per-user LaunchAgent
    Uninstall,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Completions { shell } => {
            print_completion_script(*shell);
            return Ok(());
        }
        Commands::Complete { kind, prefix } => {
            print_completion_candidates(*kind, prefix.as_deref());
            return Ok(());
        }
        _ => {}
    }

    let _lock = acquire_lock()?;

    match cli.command {
        Commands::Switch { name } => {
            sync::switch_provider(&name)?;
        }
        Commands::Remove {
            name,
            force,
            keep_auth,
        } => {
            remove_profile(&name, force, keep_auth)?;
        }
        Commands::List => {
            let profiles = provider::list_profile_names()?;
            let state = sync::load_state();
            println!("Registered providers:");
            for name in &profiles {
                let marker = if state.last_profile_name.as_ref() == Some(name) {
                    " (active)"
                } else {
                    ""
                };
                println!("  - {}{}", name, marker);
            }
            auth::print_duplicate_chatgpt_auth_warnings()?;
        }
        Commands::Status => {
            sync::show_status()?;
        }
        Commands::Sync { auto } => {
            if auto {
                sync::auto_sync()?;
            } else {
                println!("Running manual sync...");
                sync::manual_sync()?;
                println!("Sync complete.");
            }
        }
        Commands::Setup { no_service } => {
            setup::run_setup(setup::SetupOptions {
                install_service: !no_service,
            })?;
        }
        Commands::Doctor => {
            setup::run_doctor()?;
        }
        Commands::Service { command } => match command {
            ServiceCommand::Install => service::install_launch_agent()?,
            ServiceCommand::Status => service::show_launch_agent_status()?,
            ServiceCommand::Uninstall => service::uninstall_launch_agent()?,
        },
        Commands::Init => {
            migrate::init_migrate()?;
        }
        Commands::ImportAuth { name, all } => {
            if all {
                import_all_auth()?;
            } else if let Some(name) = name {
                import_single_auth(&name)?;
            } else {
                println!("Specify --all or a provider name.");
            }
        }
        Commands::Login {
            name,
            model,
            model_provider,
            switch,
        } => {
            let saved = oauth::login_and_save(oauth::LoginOptions {
                name,
                model,
                model_provider,
                switch_after: switch,
            })?;
            println!("Saved profile: {}", saved.profile_name);
            println!("Auth snapshot: {}", saved.auth_snapshot.display());
            if !switch {
                println!("Switch to it with: ucp switch {}", saved.profile_name);
            }
        }
        Commands::Add {
            name,
            model_provider,
            model,
            base_url,
            api_key,
            wire_api,
            context_window,
            auto_compact_limit,
        } => {
            let mut auth_map = std::collections::HashMap::new();
            if let Some(api_key) = api_key {
                auth_map.insert("OPENAI_API_KEY".to_string(), api_key);
            }
            let profile = provider::ProviderProfile {
                provider: provider::ProviderConfig {
                    model_provider,
                    name: name.clone(),
                    model,
                    base_url,
                    wire_api: wire_api.or_else(|| Some("responses".to_string())),
                    requires_openai_auth: None,
                    model_context_window: context_window,
                    model_auto_compact_token_limit: auto_compact_limit,
                    model_reasoning_effort: None,
                    disable_response_storage: None,
                },
                auth: auth_map,
                config_overrides: None,
            };
            provider::save_profile(&name, &profile)?;
            println!("Profile '{}' created.", name);
        }
        Commands::Completions { .. } | Commands::Complete { .. } => {}
    }
    Ok(())
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum CompleteKind {
    Profile,
}

fn print_completion_candidates(kind: CompleteKind, prefix: Option<&str>) {
    match kind {
        CompleteKind::Profile => {
            let Ok(names) = provider::list_profile_file_names() else {
                return;
            };
            let prefix = prefix.unwrap_or_default();
            for name in names {
                if name.starts_with(prefix) {
                    println!("{name}");
                }
            }
        }
    }
}

fn print_completion_script(shell: CompletionShell) {
    match shell {
        CompletionShell::Bash => print!("{}", BASH_COMPLETION),
        CompletionShell::Zsh => print!("{}", ZSH_COMPLETION),
        CompletionShell::Fish => print!("{}", FISH_COMPLETION),
    }
}

fn remove_profile(name: &str, force: bool, keep_auth: bool) -> Result<()> {
    let state = sync::load_state();
    let is_active = state.last_profile_name.as_deref() == Some(name);
    if is_active && !force {
        anyhow::bail!(
            "Refusing to remove active profile '{}'. Switch to another profile first, or pass --force.",
            name
        );
    }

    let profile_path = provider::delete_profile(name)?;
    println!("Removed profile: {}", profile_path.display());

    let auth_path = auth::auth_snapshot_path(name);
    if keep_auth {
        println!("Kept auth snapshot: {}", auth_path.display());
    } else if auth_path.exists() {
        std::fs::remove_file(&auth_path)?;
        println!("Removed auth snapshot: {}", auth_path.display());
    } else {
        println!("No auth snapshot found: {}", auth_path.display());
    }

    if is_active {
        sync::clear_state()?;
        println!("Cleared UCP state for removed active profile.");
        println!(
            "Current config.toml/auth.json were left untouched; switch to another profile next."
        );
    }

    Ok(())
}

const BASH_COMPLETION: &str = r#"_ucp()
{
    local cur prev cmd
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    if [[ ${COMP_CWORD} -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "status list switch remove delete rm add init import-auth login sync setup doctor service completions" -- "${cur}") )
        return 0
    fi

    cmd="${COMP_WORDS[1]}"
    case "${cmd}" in
        switch)
            if [[ ${COMP_CWORD} -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$(ucp __complete profile "${cur}" 2>/dev/null)" -- "${cur}") )
            fi
            ;;
        remove|delete|rm)
            if [[ "${cur}" == -* ]]; then
                COMPREPLY=( $(compgen -W "--force --keep-auth" -- "${cur}") )
            elif [[ ${COMP_CWORD} -eq 2 ]]; then
                COMPREPLY=( $(compgen -W "$(ucp __complete profile "${cur}" 2>/dev/null)" -- "${cur}") )
            fi
            ;;
        import-auth)
            if [[ "${cur}" == -* ]]; then
                COMPREPLY=( $(compgen -W "--all" -- "${cur}") )
            else
                COMPREPLY=( $(compgen -W "$(ucp __complete profile "${cur}" 2>/dev/null)" -- "${cur}") )
            fi
            ;;
        completions)
            COMPREPLY=( $(compgen -W "bash zsh fish" -- "${cur}") )
            ;;
        sync)
            COMPREPLY=( $(compgen -W "--auto" -- "${cur}") )
            ;;
        setup)
            COMPREPLY=( $(compgen -W "--no-service" -- "${cur}") )
            ;;
        service)
            COMPREPLY=( $(compgen -W "install status uninstall" -- "${cur}") )
            ;;
        login)
            COMPREPLY=( $(compgen -W "--name --model --model-provider --switch" -- "${cur}") )
            ;;
        add)
            COMPREPLY=( $(compgen -W "--model-provider --model --base-url --api-key --wire-api --context-window --auto-compact-limit" -- "${cur}") )
            ;;
    esac
}
complete -F _ucp ucp
"#;

const ZSH_COMPLETION: &str = r#"#compdef ucp

_ucp()
{
    local context state line
    typeset -A opt_args
    local -a commands profiles

    commands=(
        'status:Show current provider status'
        'list:List registered provider profiles'
        'switch:Switch to a provider profile'
        'remove:Remove a provider profile'
        'delete:Remove a provider profile'
        'rm:Remove a provider profile'
        'add:Add a provider profile'
        'init:Migrate legacy config files'
        'import-auth:Import auth snapshots'
        'login:Login to OpenAI and save a profile'
        'sync:Run provider/session sync'
        'setup:Run first-time setup'
        'doctor:Diagnose local environment'
        'service:Manage macOS auto-sync service'
        'completions:Generate shell completion script'
    )

    _arguments -C \
        '1:command:->command' \
        '*::arg:->arg' && return

    case "$state" in
        command)
            _describe -t commands 'ucp command' commands
            ;;
        arg)
            case "$line[1]" in
                switch)
                    profiles=("${(@f)$(_call_program profiles ucp __complete profile "$PREFIX" 2>/dev/null)}")
                    _describe -t profiles 'provider profile' profiles
                    ;;
                remove|delete|rm)
                    if [[ "$words[CURRENT]" == -* ]]; then
                        _arguments \
                            '--force[Allow removing the active profile]' \
                            '--keep-auth[Keep auth snapshot]'
                    else
                        profiles=("${(@f)$(_call_program profiles ucp __complete profile "$PREFIX" 2>/dev/null)}")
                        _describe -t profiles 'provider profile' profiles
                    fi
                    ;;
                import-auth)
                    if [[ "$words[CURRENT]" == -* ]]; then
                        _arguments '--all[Import auth for all profiles]'
                    else
                        profiles=("${(@f)$(_call_program profiles ucp __complete profile "$PREFIX" 2>/dev/null)}")
                        _describe -t profiles 'provider profile' profiles
                    fi
                    ;;
                completions)
                    _values 'shell' bash zsh fish
                    ;;
                sync)
                    _arguments '--auto[Run LaunchAgent-style auto sync]'
                    ;;
                setup)
                    _arguments '--no-service[Skip macOS LaunchAgent installation]'
                    ;;
                service)
                    _values 'service command' install status uninstall
                    ;;
                login)
                    _arguments \
                        '--name[Profile name]:name:' \
                        '--model[Model name]:model:' \
                        '--model-provider[Runtime model provider key]:model provider:' \
                        '--switch[Switch after login]'
                    ;;
                add)
                    _arguments \
                        '--model-provider[Runtime model provider key]:model provider:' \
                        '--model[Model name]:model:' \
                        '--base-url[API base URL]:url:' \
                        '--api-key[API key]:key:' \
                        '--wire-api[Wire API]:wire api:(responses chat)' \
                        '--context-window[Context window tokens]:tokens:' \
                        '--auto-compact-limit[Auto compact token limit]:tokens:'
                    ;;
            esac
            ;;
    esac
}

_ucp "$@"
"#;

const FISH_COMPLETION: &str = r#"complete -c ucp -f
complete -c ucp -n '__fish_is_first_arg' -a 'status' -d 'Show current provider status'
complete -c ucp -n '__fish_is_first_arg' -a 'list' -d 'List registered provider profiles'
complete -c ucp -n '__fish_is_first_arg' -a 'switch' -d 'Switch to a provider profile'
complete -c ucp -n '__fish_is_first_arg' -a 'remove delete rm' -d 'Remove a provider profile'
complete -c ucp -n '__fish_is_first_arg' -a 'add' -d 'Add a provider profile'
complete -c ucp -n '__fish_is_first_arg' -a 'init' -d 'Migrate legacy config files'
complete -c ucp -n '__fish_is_first_arg' -a 'import-auth' -d 'Import auth snapshots'
complete -c ucp -n '__fish_is_first_arg' -a 'login' -d 'Login to OpenAI and save a profile'
complete -c ucp -n '__fish_is_first_arg' -a 'sync' -d 'Run provider/session sync'
complete -c ucp -n '__fish_is_first_arg' -a 'setup' -d 'Run first-time setup'
complete -c ucp -n '__fish_is_first_arg' -a 'doctor' -d 'Diagnose local environment'
complete -c ucp -n '__fish_is_first_arg' -a 'service' -d 'Manage macOS auto-sync service'
complete -c ucp -n '__fish_is_first_arg' -a 'completions' -d 'Generate shell completion script'
complete -c ucp -n '__fish_seen_subcommand_from switch' -a '(ucp __complete profile (commandline -ct) 2>/dev/null)' -d 'Provider profile'
complete -c ucp -n '__fish_seen_subcommand_from remove delete rm; and not string match -q -- "-*" (commandline -ct)' -a '(ucp __complete profile (commandline -ct) 2>/dev/null)' -d 'Provider profile'
complete -c ucp -n '__fish_seen_subcommand_from remove delete rm' -l force -d 'Allow removing the active profile'
complete -c ucp -n '__fish_seen_subcommand_from remove delete rm' -l keep-auth -d 'Keep auth snapshot'
complete -c ucp -n '__fish_seen_subcommand_from import-auth' -l all -d 'Import auth for all profiles'
complete -c ucp -n '__fish_seen_subcommand_from import-auth; and not string match -q -- "-*" (commandline -ct)' -a '(ucp __complete profile (commandline -ct) 2>/dev/null)' -d 'Provider profile'
complete -c ucp -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'
complete -c ucp -n '__fish_seen_subcommand_from sync' -l auto -d 'Run LaunchAgent-style auto sync'
complete -c ucp -n '__fish_seen_subcommand_from setup' -l no-service -d 'Skip macOS LaunchAgent installation'
complete -c ucp -n '__fish_seen_subcommand_from service' -a 'install status uninstall'
complete -c ucp -n '__fish_seen_subcommand_from login' -l name -d 'Profile name' -r
complete -c ucp -n '__fish_seen_subcommand_from login' -l model -d 'Model name' -r
complete -c ucp -n '__fish_seen_subcommand_from login' -l model-provider -d 'Runtime model provider key' -r
complete -c ucp -n '__fish_seen_subcommand_from login' -l switch -d 'Switch after login'
complete -c ucp -n '__fish_seen_subcommand_from add' -l model-provider -d 'Runtime model provider key' -r
complete -c ucp -n '__fish_seen_subcommand_from add' -l model -d 'Model name' -r
complete -c ucp -n '__fish_seen_subcommand_from add' -l base-url -d 'API base URL' -r
complete -c ucp -n '__fish_seen_subcommand_from add' -l api-key -d 'API key' -r
complete -c ucp -n '__fish_seen_subcommand_from add' -l wire-api -d 'Wire API' -a 'responses chat'
complete -c ucp -n '__fish_seen_subcommand_from add' -l context-window -d 'Context window tokens' -r
complete -c ucp -n '__fish_seen_subcommand_from add' -l auto-compact-limit -d 'Auto compact token limit' -r
"#;

fn acquire_lock() -> Result<std::fs::File> {
    use fs2::FileExt;
    use std::io::Write;
    let codex_dir = config::codex_dir();
    std::fs::create_dir_all(&codex_dir)?;
    let lock_path = codex_dir.join(".ucp.lock");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)?;
    f.lock_exclusive()?;
    let _ = writeln!(f, "{}", std::process::id());
    Ok(f)
}

fn import_single_auth(name: &str) -> Result<()> {
    let codex = config::codex_dir();
    let src = codex.join(format!("auth_{}.json", name));
    let dst = codex.join("providers").join(format!("{}.auth.json", name));
    if !src.exists() {
        anyhow::bail!("Source file not found: {}", src.display());
    }
    std::fs::copy(&src, &dst)?;
    println!("Updated snapshot: {} -> {}", src.display(), dst.display());
    Ok(())
}

fn import_all_auth() -> Result<()> {
    let codex = config::codex_dir();
    let profiles = provider::list_profile_names()?;
    for entry in std::fs::read_dir(&codex)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname.starts_with("auth_") && fname.ends_with(".json") {
            let profile_name = fname
                .strip_prefix("auth_")
                .unwrap()
                .strip_suffix(".json")
                .unwrap()
                .to_string();
            if profiles.contains(&profile_name) {
                let dst = codex
                    .join("providers")
                    .join(format!("{}.auth.json", &profile_name));
                std::fs::copy(entry.path(), &dst)?;
                println!("Updated: {}", profile_name);
            } else {
                println!("Skipped (no profile): {}", profile_name);
            }
        }
    }
    Ok(())
}

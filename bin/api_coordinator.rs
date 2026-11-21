//! Operator commands for personal coordinator accounts in the runtime image.

use std::io::{self, Write};

use anyhow::{Context, ensure};
use dashboard_api::coordinator_accounts::{
    AccountSpec, IssuedCredentials, create_accounts, disable_account, has_enabled_account,
};
use dashboard_store::{Pool, read_repo};

const TEST_ACCOUNTS: [AccountSpec<'static>; 3] = [
    AccountSpec {
        login: "test-coordinator-1",
        display_name: "Тестовый координатор 1",
    },
    AccountSpec {
        login: "test-coordinator-2",
        display_name: "Тестовый координатор 2",
    },
    AccountSpec {
        login: "test-coordinator-3",
        display_name: "Тестовый координатор 3",
    },
];

enum Command<'a> {
    Create(&'a str, &'a str),
    CreateTest,
    Disable(&'a str),
    Check,
}

pub(super) async fn run(arguments: Vec<String>) -> anyhow::Result<()> {
    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let command = match arguments.as_slice() {
        ["create", login] => Command::Create(login, login),
        ["create", login, display_name] => Command::Create(login, display_name),
        ["create-test"] => Command::CreateTest,
        ["disable", login] => Command::Disable(login),
        ["check"] => Command::Check,
        _ => anyhow::bail!(
            "usage: api coordinator create LOGIN [DISPLAY_NAME] | create-test | disable LOGIN | check"
        ),
    };

    let settings = crate::common::load_settings();
    let pool = dashboard_store::connect(&settings)
        .await
        .context("cannot connect to PostgreSQL")?;
    match command {
        Command::Create(login, display_name) => {
            let created = create_accounts(&pool, &[AccountSpec { login, display_name }]).await?;
            write_credentials(&created)
        }
        Command::CreateTest => {
            let created = create_accounts(&pool, &TEST_ACCOUNTS).await?;
            write_credentials(&created)
        }
        Command::Disable(login) => {
            ensure!(
                disable_account(&pool, login).await?,
                "no active coordinator with this login"
            );
            Ok(())
        }
        Command::Check => {
            ensure!(
                settings.coordinator_accounts_enabled,
                "coordinator account mode is disabled"
            );
            check(&pool).await
        }
    }
}

fn write_credentials(created: &[IssuedCredentials]) -> anyhow::Result<()> {
    let json = serde_json::to_vec(created).context("cannot encode issued coordinator credentials")?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&json)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

async fn check(pool: &Pool) -> anyhow::Result<()> {
    // Provisioning and revocation can intentionally leave no active accounts.
    has_enabled_account(pool)
        .await
        .context("cannot read coordinator account schema")?;
    let mut read = read_repo::begin_consistent_read(pool).await?;
    let state = read_repo::import_state(&mut read).await?;
    ensure!(
        state.is_some_and(|state| state.status == "ok" && state.last_success_at.is_some()),
        "incident source has no successful current import"
    );
    Ok(())
}

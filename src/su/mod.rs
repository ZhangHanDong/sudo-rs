use crate::common::error::Error;
use crate::exec::{ExitReason, RunOptions};
use crate::log::user_warn;
use crate::pam::{CLIConverser, PamContext, PamError, PamErrorType};
use std::{env, process};

use cli::{SuAction, SuOptions};
use context::SuContext;

mod cli;
mod context;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn authenticate(user: &str, login: bool) -> Result<PamContext<CLIConverser>, Error> {
    let context = if login { "su-l" } else { "su" };
    let use_stdin = true;
    let mut pam = PamContext::builder_cli("su", use_stdin, Default::default())
        .target_user(user)
        .service_name(context)
        .build()?;

    pam.mark_silent(true);
    pam.mark_allow_null_auth_token(false);

    pam.set_user(user)?;

    let mut max_tries = 3;
    let mut current_try = 0;

    loop {
        current_try += 1;
        match pam.authenticate() {
            // there was no error, so authentication succeeded
            Ok(_) => break,

            // maxtries was reached, pam does not allow any more tries
            Err(PamError::Pam(PamErrorType::MaxTries, _)) => {
                return Err(Error::MaxAuthAttempts(current_try));
            }

            // there was an authentication error, we can retry
            Err(PamError::Pam(PamErrorType::AuthError, _)) => {
                max_tries -= 1;
                if max_tries == 0 {
                    return Err(Error::MaxAuthAttempts(current_try));
                } else {
                    user_warn!("Authentication failed, try again.");
                }
            }

            // there was another pam error, return the error
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    pam.validate_account_or_change_auth_token()?;
    pam.open_session()?;

    Ok(pam)
}

fn run(options: SuOptions) -> Result<(), Error> {
    // lookup user and build context object
    let context = SuContext::from_env(options)?;

    // authenticate the target user
    let mut pam = authenticate(&context.user().name, context.is_login())?;

    // run command and return corresponding exit code
    let environment = context.environment.clone();
    let pid = context.process.pid;

    let (reason, emulate_default_handler) = crate::exec::run_command(context, environment)?;

    // closing the pam session is best effort, if any error occurs we cannot
    // do anything with it
    let _ = pam.close_session();

    // Run any clean-up code before this line.
    emulate_default_handler();

    match reason {
        ExitReason::Code(code) => process::exit(code),
        ExitReason::Signal(signal) => {
            crate::system::kill(pid, signal)?;
        }
    }

    Ok(())
}

pub fn main() {
    let su_options = SuOptions::from_env().unwrap();

    match su_options.action {
        SuAction::Help => {
            println!("Usage: su [options] [-] [<user> [<argument>...]]");
            std::process::exit(0);
        }
        SuAction::Version => {
            eprintln!("su-rs {VERSION}");
            std::process::exit(0);
        }
        SuAction::Run => match run(su_options) {
            Err(Error::CommandNotFound(c)) => {
                eprintln!("su: {}", Error::CommandNotFound(c));
                std::process::exit(127);
            }
            Err(Error::InvalidCommand(c)) => {
                eprintln!("su: {}", Error::InvalidCommand(c));
                std::process::exit(126);
            }
            Err(e) => {
                eprintln!("su: {e}");
                std::process::exit(1);
            }
            _ => {}
        },
    };
}

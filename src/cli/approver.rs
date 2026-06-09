use std::io::{self, Write};

use crate::domain::approval::{ApprovalRequest, Approver, Risk};

/// Interactive approver: prints the action and reads a y/N answer from stdin.
pub struct CliApprover;

impl Approver for CliApprover {
    fn approve(&self, request: &ApprovalRequest) -> bool {
        match request.risk {
            Risk::Dangerous => {
                print!("\n🛑 DANGEROUS — request to {}", request.summary);
                if let Some(detail) = &request.detail {
                    print!("\n   ({detail})");
                }
                print!("\n   Approve anyway? [y/N] ");
            }
            Risk::Normal => {
                print!("\n⚠  Approve request to {}? [y/N] ", request.summary);
            }
        }

        if io::stdout().flush().is_err() {
            return false;
        }

        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            return false;
        }

        matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
    }
}

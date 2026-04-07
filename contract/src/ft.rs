//! NEP-141 Fungible Token receiver and balance tracking.
//!
//! Allows the multisig contract to receive, hold, and send FTs (USDC, USDT, etc.)
//! per wallet. Each wallet tracks its own FT balances independently.

use crate::*;

/// FT balance key: wallet_name + token_account_id
fn ft_balance_key(wallet: &str, token: &str) -> String {
    format!("{}:ft:{}", wallet, token)
}

#[near_bindgen]
impl Contract {
    /// NEP-141 `ft_on_transfer` callback.
    /// Callers transfer FTs to this contract with `msg` = wallet name.
    /// Returns unused amount (always "0" — we accept all tokens).
    ///
    /// Example: `ft_transfer_call(contract_id, "1000000", "treasury")`
    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        let token_account = env::predecessor_account_id();
        let wallet_name = if msg.is_empty() {
            // If no msg, try to credit sender's wallet (if they have one named after them)
            sender_id.as_str().to_string()
        } else {
            msg
        };

        // Verify wallet exists
        assert!(
            self.wallets.get(&wallet_name).is_some(),
            "ERR_WALLET_NOT_FOUND: '{}'",
            wallet_name
        );

        // Credit the balance
        let bkey = ft_balance_key(&wallet_name, token_account.as_str());
        let current: u128 = env::storage_read(&bkey.as_bytes())
            .map(|bytes| {
                let arr: [u8; 16] = bytes.try_into().unwrap_or([0u8; 16]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0);

        let new_balance = current + amount.0;
        env::storage_write(&bkey.as_bytes(), &new_balance.to_le_bytes());

        self.emit("ft_received", serde_json::json!({
            "wallet": wallet_name,
            "token": token_account.to_string(),
            "amount": amount.0.to_string(),
            "sender": sender_id.to_string(),
            "new_balance": new_balance.to_string(),
        }));

        log!(
            "FT received: {} tokens from {} for wallet '{}' (balance: {})",
            amount.0,
            sender_id,
            wallet_name,
            new_balance
        );

        // Return 0 = we accept all tokens (no refund)
        PromiseOrValue::Value(U128(0))
    }

    // ── FT Balance Views ───────────────────────────────────────────────

    /// Get FT balance for a specific token in a wallet.
    pub fn get_ft_balance(&self, wallet_name: String, token: AccountId) -> U128 {
        assert!(
            self.wallets.get(&wallet_name).is_some(),
            "ERR_WALLET_NOT_FOUND"
        );
        let bkey = ft_balance_key(&wallet_name, token.as_str());
        let balance: u128 = env::storage_read(&bkey.as_bytes())
            .map(|bytes| {
                let arr: [u8; 16] = bytes.try_into().unwrap_or([0u8; 16]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0);
        U128(balance)
    }

    /// Get NEAR balance held by the contract for a specific wallet.
    /// Note: this is tracked internally, not the raw contract balance.
    pub fn get_wallet_near_balance(&self, wallet_name: String) -> U128 {
        assert!(
            self.wallets.get(&wallet_name).is_some(),
            "ERR_WALLET_NOT_FOUND"
        );
        let bkey = format!("{}:near", wallet_name);
        let balance: u128 = env::storage_read(&bkey.as_bytes())
            .map(|bytes| {
                let arr: [u8; 16] = bytes.try_into().unwrap_or([0u8; 16]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0);
        U128(balance)
    }
}

// ── Internal FT Operations (used by execute.rs) ────────────────────────────

impl Contract {
    /// Credit NEAR to a wallet's internal balance (called during deposit operations).
    pub(crate) fn credit_near(&mut self, wallet_name: &str, amount: u128) {
        let bkey = format!("{}:near", wallet_name);
        let current: u128 = env::storage_read(bkey.as_bytes())
            .map(|bytes| {
                let arr: [u8; 16] = bytes.try_into().unwrap_or([0u8; 16]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0);
        let new_balance = current + amount;
        env::storage_write(bkey.as_bytes(), &new_balance.to_le_bytes());
    }

    /// Debit NEAR from a wallet's internal balance. Panics if insufficient.
    pub(crate) fn debit_near(&mut self, wallet_name: &str, amount: u128) {
        let bkey = format!("{}:near", wallet_name);
        let current: u128 = env::storage_read(bkey.as_bytes())
            .map(|bytes| {
                let arr: [u8; 16] = bytes.try_into().unwrap_or([0u8; 16]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0);
        assert!(current >= amount, "ERR_INSUFFICIENT_NEAR: have {}, need {}", current, amount);
        let new_balance = current - amount;
        env::storage_write(bkey.as_bytes(), &new_balance.to_le_bytes());
    }

    /// Debit FT from a wallet's internal balance. Panics if insufficient.
    pub(crate) fn debit_ft(&mut self, wallet_name: &str, token: &str, amount: u128) {
        let bkey = ft_balance_key(wallet_name, token);
        let current: u128 = env::storage_read(bkey.as_bytes())
            .map(|bytes| {
                let arr: [u8; 16] = bytes.try_into().unwrap_or([0u8; 16]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0);
        assert!(current >= amount, "ERR_INSUFFICIENT_FT: have {}, need {}", current, amount);
        let new_balance = current - amount;
        env::storage_write(bkey.as_bytes(), &new_balance.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ft_balance_key() {
        assert_eq!(ft_balance_key("treasury", "usdt.tether-token.near"), "treasury:ft:usdt.tether-token.near");
        assert_eq!(ft_balance_key("a", "b"), "a:ft:b");
    }
}

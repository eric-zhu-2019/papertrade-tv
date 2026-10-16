//! Users, sessions, API keys and one-time invite codes.

use crate::engine::Engine;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::{Rng, RngCore};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub is_admin: bool,
}

pub fn hash_password(pw: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default().hash_password(pw.as_bytes(), &salt).expect("hash").to_string()
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash).map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok()).unwrap_or(false)
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn valid_username(u: &str) -> bool {
    let n = u.chars().count();
    (2..=20).contains(&n) && u.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

const SESSION_MS: i64 = 30 * 86_400_000;

/// Characters for one-time invite codes: no 0/O or 1/I/L, so a code read aloud or off paper is typed right.
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Canonical form of a typed invite code: "abcd efgh" and "ABCDEFGH" both become "ABCD-EFGH".
pub fn normalize_invite(code: &str) -> String {
    let s: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    if s.len() == 8 {
        format!("{}-{}", &s[..4], &s[4..])
    } else {
        s
    }
}

impl Engine {
    pub fn user_count(&self) -> i64 {
        self.db.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn create_user(&mut self, username: &str, pw_hash: &str, is_admin: bool, now: i64) -> Result<i64, String> {
        let exists: Option<i64> = self
            .db
            .query_row("SELECT id FROM users WHERE username=?", [username], |r| r.get(0))
            .optional()
            .unwrap();
        if exists.is_some() {
            return Err("that username is taken".into());
        }
        self.db
            .execute(
                "INSERT INTO users(username, pw_hash, is_admin, created_ms) VALUES(?,?,?,?)",
                params![username, pw_hash, is_admin as i64, now],
            )
            .map_err(|e| e.to_string())?;
        let id = self.db.last_insert_rowid();
        self.grant(id, now);
        self.commit();
        Ok(id)
    }

    /// (id, pw_hash, disabled) for a login attempt.
    pub fn login_lookup(&self, username: &str) -> Option<(i64, String, bool)> {
        self.db
            .query_row("SELECT id, pw_hash, disabled FROM users WHERE username=?", [username], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0))
            })
            .optional()
            .unwrap()
    }

    pub fn login_blocked(&self, username: &str, now: i64) -> bool {
        matches!(self.login_fails.get(&username.to_lowercase()), Some((n, until)) if *n >= 8 && *until > now)
    }

    pub fn login_result(&mut self, username: &str, ok: bool, now: i64) {
        let key = username.to_lowercase();
        if ok {
            self.login_fails.remove(&key);
        } else {
            let e = self.login_fails.entry(key).or_insert((0, 0));
            if e.1 < now {
                e.0 = 0;
            }
            e.0 += 1;
            e.1 = now + 10 * 60_000;
        }
    }

    pub fn set_password(&mut self, uid: i64, pw_hash: &str) {
        self.db.execute("UPDATE users SET pw_hash=? WHERE id=?", params![pw_hash, uid]).unwrap();
        self.db.execute("DELETE FROM tokens WHERE user_id=? AND kind='session'", [uid]).unwrap();
    }

    pub fn set_disabled(&mut self, uid: i64, disabled: bool) {
        self.db.execute("UPDATE users SET disabled=? WHERE id=?", params![disabled as i64, uid]).unwrap();
        if disabled {
            self.db.execute("DELETE FROM tokens WHERE user_id=?", [uid]).unwrap();
        }
    }

    /// Create a session token or an API key. The plaintext is returned once; only its hash is stored.
    pub fn create_token(&mut self, uid: i64, kind: &str, label: &str, now: i64) -> String {
        let mut raw = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut raw);
        let token = format!("{}{}", if kind == "api" { "pt_" } else { "s_" }, hex::encode(raw));
        let expires = if kind == "session" { Some(now + SESSION_MS) } else { None };
        self.db
            .execute(
                "INSERT INTO tokens(hash, user_id, kind, label, prefix, created_ms, expires_ms) VALUES(?,?,?,?,?,?,?)",
                params![token_hash(&token), uid, kind, label, &token[..10], now, expires],
            )
            .unwrap();
        token
    }

    pub fn auth(&self, token: &str, now: i64) -> Option<User> {
        self.db
            .query_row(
                "SELECT u.id, u.username, u.is_admin FROM tokens t JOIN users u ON u.id=t.user_id
                 WHERE t.hash=? AND u.disabled=0 AND (t.expires_ms IS NULL OR t.expires_ms>?)",
                params![token_hash(token), now],
                |r| Ok(User { id: r.get(0)?, username: r.get(1)?, is_admin: r.get::<_, i64>(2)? != 0 }),
            )
            .optional()
            .unwrap()
    }

    pub fn delete_token(&mut self, token: &str) {
        self.db.execute("DELETE FROM tokens WHERE hash=?", [token_hash(token)]).unwrap();
    }

    pub fn list_api_keys(&self, uid: i64) -> Vec<Value> {
        let mut st = self
            .db
            .prepare("SELECT prefix, label, created_ms FROM tokens WHERE user_id=? AND kind='api' ORDER BY created_ms")
            .unwrap();
        let rows = st
            .query_map([uid], |r| {
                Ok(json!({"prefix": r.get::<_, String>(0)?, "label": r.get::<_, Option<String>>(1)?, "created_ms": r.get::<_, i64>(2)?}))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    pub fn delete_api_key(&mut self, uid: i64, prefix: &str) {
        self.db.execute("DELETE FROM tokens WHERE user_id=? AND kind='api' AND prefix=?", params![uid, prefix]).unwrap();
    }

    pub fn create_invites(&mut self, n: usize, note: &str, now: i64) -> Vec<String> {
        let mut rng = rand::thread_rng();
        let mut codes = Vec::with_capacity(n);
        while codes.len() < n {
            let s: String = (0..8).map(|_| INVITE_ALPHABET[rng.gen_range(0..INVITE_ALPHABET.len())] as char).collect();
            let code = format!("{}-{}", &s[..4], &s[4..]);
            // OR IGNORE: on the (vanishingly rare) duplicate, just draw again
            let added = self
                .db
                .execute("INSERT OR IGNORE INTO invites(code, note, created_ms) VALUES(?,?,?)", params![code, note, now])
                .unwrap();
            if added == 1 {
                codes.push(code);
            }
        }
        codes
    }

    /// `code` must already be normalized.
    pub fn invite_usable(&self, code: &str) -> bool {
        self.db
            .query_row("SELECT 1 FROM invites WHERE code=? AND used_by IS NULL", [code], |_| Ok(()))
            .optional()
            .unwrap()
            .is_some()
    }

    pub fn unused_invites(&self) -> i64 {
        self.db.query_row("SELECT COUNT(*) FROM invites WHERE used_by IS NULL", [], |r| r.get(0)).unwrap_or(0)
    }

    /// Create a student account with a one-time code. The code is used up only if the account is created,
    /// so a taken nickname does not waste it. Callers hold the engine lock, so two people cannot redeem it at once.
    pub fn register_with_invite(&mut self, code: &str, username: &str, pw_hash: &str, now: i64) -> Result<i64, String> {
        if !self.invite_usable(code) {
            return Err("wrong or already used invite code".into());
        }
        let uid = self.create_user(username, pw_hash, false, now)?;
        self.db.execute("UPDATE invites SET used_by=?, used_ms=? WHERE code=?", params![uid, now, code]).unwrap();
        Ok(uid)
    }

    /// Unused codes first, newest first.
    pub fn list_invites(&self) -> Vec<Value> {
        let mut st = self
            .db
            .prepare(
                "SELECT i.code, i.note, i.created_ms, i.used_by, u.username, i.used_ms FROM invites i
                 LEFT JOIN users u ON u.id=i.used_by ORDER BY i.used_by IS NOT NULL, i.created_ms DESC, i.code",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok(json!({
                    "code": r.get::<_, String>(0)?, "note": r.get::<_, Option<String>>(1)?, "created_ms": r.get::<_, i64>(2)?,
                    "used_by": r.get::<_, Option<i64>>(3)?, "used_by_name": r.get::<_, Option<String>>(4)?,
                    "used_ms": r.get::<_, Option<i64>>(5)?,
                }))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    pub fn revoke_invite(&mut self, code: &str) -> Result<(), String> {
        match self.db.execute("DELETE FROM invites WHERE code=? AND used_by IS NULL", [code]).unwrap() {
            0 => Err("no unused invite code like that".into()),
            _ => Ok(()),
        }
    }

    pub fn list_users(&self) -> Vec<Value> {
        let rows: Vec<(i64, String, bool, bool, i64)> = {
            let mut st = self.db.prepare("SELECT id, username, is_admin, disabled, created_ms FROM users ORDER BY id").unwrap();
            let rows = st
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get::<_, i64>(3)? != 0, r.get(4)?))
                })
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        rows.into_iter()
            .map(|(id, name, admin, disabled, created)| {
                json!({"id": id, "username": name, "is_admin": admin, "disabled": disabled, "created_ms": created,
                       "equity": self.equity(id).round_dp(2).to_string(),
                       "open_orders": self.orders.values().filter(|o| o.user_id == id).count()})
            })
            .collect()
    }
}

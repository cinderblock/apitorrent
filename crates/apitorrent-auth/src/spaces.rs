//! Space memberships: a role per user and space, each user's last space, and the users a
//! server admin picks members from. The spaces themselves live in the instance database;
//! here they are only ids.
//!
//! A server admin (the instance's `admin` role) and the local owner act as `admin` in every
//! space without a membership. An API token works only in the space it was made in.

use rusqlite::{OptionalExtension, params};

use apitorrent_types::spaces::{SpaceId, SpaceMember, SpaceRole};

use crate::{
    Auth, AuthError, ClientMeta, Credential, Principal, Result, Role, TokenScope, UserInfo, store,
};

const ADMINS_ONLY: AuthError = AuthError::Forbidden("only a server admin manages members");

impl Principal {
    /// The instance's admin: every space, its members, new spaces, instance settings.
    pub fn server_admin(&self) -> bool {
        matches!(self.credential, Credential::Local) || self.user.role == Role::Admin
    }

    /// The space an API token is bound to, for token credentials.
    pub fn token_space(&self) -> Option<SpaceId> {
        match &self.credential {
            Credential::Token { space, .. } => Some(*space),
            _ => None,
        }
    }
}

impl Auth {
    /// The spaces a user belongs to, with their role in each, by space id.
    pub fn memberships(&self, user_id: &str) -> Result<Vec<(SpaceId, SpaceRole)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT space_id, role FROM space_members WHERE user_id = ?1 ORDER BY space_id",
        )?;
        let rows = stmt
            .query_map([user_id], |r| {
                Ok((r.get::<_, SpaceId>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(s, r)| SpaceRole::parse(&r).map(|r| (s, r)))
            .collect())
    }

    /// What a caller may do in a space, or `None` if it is no member there. A token bound
    /// to another space is never a member; a read-only token is at most a viewer.
    pub fn space_role(&self, p: &Principal, space: SpaceId) -> Result<Option<SpaceRole>> {
        if p.token_space().is_some_and(|s| s != space) {
            return Ok(None);
        }
        let role = if p.server_admin() {
            Some(SpaceRole::Admin)
        } else {
            let conn = self.db();
            conn.query_row(
                "SELECT role FROM space_members WHERE user_id = ?1 AND space_id = ?2",
                params![p.user.id, space],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|r| SpaceRole::parse(&r))
        };
        let read_only = matches!(
            p.credential,
            Credential::Token {
                scope: TokenScope::Read,
                ..
            }
        );
        Ok(role.map(|r| if read_only { SpaceRole::Viewer } else { r }))
    }

    /// A space's members, by name.
    pub fn members(&self, space: SpaceId) -> Result<Vec<SpaceMember>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT u.id, u.name, u.display_name, m.role
             FROM space_members m JOIN users u ON u.id = m.user_id
             WHERE m.space_id = ?1 ORDER BY u.name",
        )?;
        let rows = stmt
            .query_map([space], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(user_id, name, display_name, role)| {
                Some(SpaceMember {
                    user_id,
                    name,
                    display_name,
                    role: SpaceRole::parse(&role)?,
                })
            })
            .collect())
    }

    /// Give a user a role in a space (or change it). Server admins only; audited.
    pub fn set_member(
        &self,
        actor: &Principal,
        space: SpaceId,
        user_id: &str,
        role: SpaceRole,
        meta: &ClientMeta,
    ) -> Result<()> {
        if !actor.server_admin() {
            return Err(ADMINS_ONLY);
        }
        let conn = self.db();
        if store::user_by_id(&conn, user_id)?.is_none() {
            return Err(AuthError::NotFound);
        }
        conn.execute(
            "INSERT INTO space_members (user_id, space_id, role) VALUES (?1, ?2, ?3)
             ON CONFLICT (user_id, space_id) DO UPDATE SET role = excluded.role",
            params![user_id, space, role.as_str()],
        )?;
        self.log_with(
            &conn,
            "member_set",
            Some(&actor.user.id),
            meta,
            &format!("{user_id} in space {space}: {}", role.as_str()),
        );
        Ok(())
    }

    /// Take a user out of a space. Their tokens bound to it stop working. Server admins
    /// only; audited.
    pub fn remove_member(
        &self,
        actor: &Principal,
        space: SpaceId,
        user_id: &str,
        meta: &ClientMeta,
    ) -> Result<()> {
        if !actor.server_admin() {
            return Err(ADMINS_ONLY);
        }
        let conn = self.db();
        let removed = conn.execute(
            "DELETE FROM space_members WHERE user_id = ?1 AND space_id = ?2",
            params![user_id, space],
        )?;
        if removed == 0 {
            return Err(AuthError::NotFound);
        }
        let tokens = conn.execute(
            "DELETE FROM api_tokens WHERE user_id = ?1 AND space_id = ?2",
            params![user_id, space],
        )?;
        self.log_with(
            &conn,
            "member_removed",
            Some(&actor.user.id),
            meta,
            &format!("{user_id} from space {space} ({tokens} tokens revoked)"),
        );
        drop(conn);
        if tokens > 0 {
            self.bump_revocations();
        }
        Ok(())
    }

    /// Forget a deleted space: its memberships and tokens.
    pub fn forget_space(&self, space: SpaceId) -> Result<()> {
        let conn = self.db();
        conn.execute("DELETE FROM space_members WHERE space_id = ?1", [space])?;
        let tokens = conn.execute("DELETE FROM api_tokens WHERE space_id = ?1", [space])?;
        conn.execute(
            "UPDATE users SET last_space_id = NULL WHERE last_space_id = ?1",
            [space],
        )?;
        drop(conn);
        if tokens > 0 {
            self.bump_revocations();
        }
        Ok(())
    }

    /// Every user, by name (for picking members).
    pub fn users(&self) -> Result<Vec<UserInfo>> {
        let conn = self.db();
        let mut stmt = conn.prepare("SELECT id FROM users ORDER BY name")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(u) = store::user_by_id(&conn, &id)? {
                out.push(u.info());
            }
        }
        Ok(out)
    }

    /// The space a user used last, if any.
    pub fn last_space(&self, user_id: &str) -> Result<Option<SpaceId>> {
        Ok(self
            .db()
            .query_row(
                "SELECT last_space_id FROM users WHERE id = ?1",
                [user_id],
                |r| r.get::<_, Option<SpaceId>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Remember the space a user is working in, so a later sign-in without one lands there.
    pub fn set_last_space(&self, user_id: &str, space: SpaceId) -> Result<()> {
        self.db().execute(
            "UPDATE users SET last_space_id = ?2 WHERE id = ?1
             AND last_space_id IS NOT ?2",
            params![user_id, space],
        )?;
        Ok(())
    }
}

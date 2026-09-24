//! SQLite implementation of the fleet store.

use crate::store::{
    CameraRecord, DeliveryView, DueDelivery, EventView, HealthHour, OrganizationRecord,
    SessionUser, SiteRecord, Store, StoreError, StoredRegistration, StoredUser, parse_ts,
    token_hash, ts,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use vms_plugin_sdk::{EventSeverity, FleetEvent, FleetEventKind};

use vms_domain::{
    AuditView, CameraTelemetryBatch, EnrollmentRequest, GatewayEnrollmentRequest, GatewayView,
    IncidentView, RecordingManifest, RecordingPolicy, Role, UserView, VideoSource,
};

fn manifest_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RecordingManifest, StoreError> {
    serde_json::from_str(row.get::<String, _>("manifest").as_str())
        .map_err(|err| StoreError::Internal(err.into()))
}

fn camera_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<CameraRecord, StoreError> {
    Ok(CameraRecord {
        id: row.get("id"),
        gateway_id: row.get("gateway_id"),
        site_id: row.get("site_id"),
        name: row.get("name"),
        manufacturer: row.get("manufacturer"),
        model: row.get("model"),
        firmware: row.get("firmware"),
        codec: row.get("codec"),
        width: row.get::<Option<i64>, _>("width").map(|v| v as u32),
        height: row.get::<Option<i64>, _>("height").map(|v| v as u32),
        first_seen: parse_ts(row.get::<String, _>("first_seen").as_str())?,
        last_seen: parse_ts(row.get::<String, _>("last_seen").as_str())?,
    })
}

fn enrollment_from_row(row: &sqlx::sqlite::SqliteRow) -> EnrollmentRequest {
    EnrollmentRequest {
        customer_id: row.get("org_id"),
        customer_name: row.get("org_name"),
        site_id: row.get("site_id"),
        site_name: row.get("site_name"),
        city: row.get("city"),
    }
}

pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if missing) and migrate. `url` is `sqlite:<path>`.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// One permanent connection: an in-memory SQLite database lives exactly as
    /// long as its connection, so the pool must never open a second one or
    /// close the first.
    #[cfg(test)]
    pub async fn in_memory() -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .min_connections(1)
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn create_enrollment(
        &self,
        token: &str,
        request: &EnrollmentRequest,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO enrollments (token_hash, org_id, org_name, site_id, site_name, city, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(token_hash(token))
        .bind(&request.customer_id)
        .bind(&request.customer_name)
        .bind(&request.site_id)
        .bind(&request.site_name)
        .bind(&request.city)
        .bind(ts(&expires_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn enrollment_request(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<EnrollmentRequest, StoreError> {
        let row = sqlx::query(
            "SELECT org_id, org_name, site_id, site_name, city, claimed, expires_at
             FROM enrollments WHERE token_hash = ?1",
        )
        .bind(token_hash(token))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        let claimed: i64 = row.get("claimed");
        let expires_at = parse_ts(row.get::<String, _>("expires_at").as_str())?;
        if claimed != 0 || expires_at < now {
            return Err(StoreError::Gone);
        }
        Ok(enrollment_from_row(&row))
    }

    async fn claim_enrollment(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<EnrollmentRequest, StoreError> {
        let claimed = sqlx::query(
            "UPDATE enrollments SET claimed = 1
             WHERE token_hash = ?1 AND claimed = 0 AND expires_at > ?2
             RETURNING org_id, org_name, site_id, site_name, city",
        )
        .bind(token_hash(token))
        .bind(ts(&now))
        .fetch_optional(&self.pool)
        .await?;
        match claimed {
            Some(row) => Ok(enrollment_from_row(&row)),
            // Distinguish "never existed" from "existed, but claimed/expired".
            None => match self.enrollment_request(token, now).await {
                Err(StoreError::NotFound) => Err(StoreError::NotFound),
                _ => Err(StoreError::Gone),
            },
        }
    }

    async fn enroll_gateway(
        &self,
        request: &EnrollmentRequest,
        enroll: &GatewayEnrollmentRequest,
        gateway_token: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO organizations (id, name) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
        )
        .bind(&request.customer_id)
        .bind(&request.customer_name)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO sites (id, org_id, name, city) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET org_id = excluded.org_id,
                 name = excluded.name, city = excluded.city",
        )
        .bind(&request.site_id)
        .bind(&request.customer_id)
        .bind(&request.site_name)
        .bind(&request.city)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO gateways (id, site_id, hostname, version, token_hash, enrolled_at, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)
             ON CONFLICT(id) DO UPDATE SET site_id = excluded.site_id,
                 hostname = excluded.hostname, version = excluded.version,
                 token_hash = excluded.token_hash, enrolled_at = excluded.enrolled_at,
                 revoked_at = NULL",
        )
        .bind(&enroll.gateway_id)
        .bind(&request.site_id)
        .bind(&enroll.hostname)
        .bind(&enroll.version)
        .bind(token_hash(gateway_token))
        .bind(ts(&now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn verify_gateway_token(
        &self,
        gateway_id: &str,
        token: &str,
    ) -> Result<bool, StoreError> {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM gateways WHERE id = ?1")
                .bind(gateway_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(stored.is_some_and(|hash| !hash.is_empty() && hash == token_hash(token)))
    }

    async fn verify_any_gateway_token(&self, token: &str) -> Result<bool, StoreError> {
        // A bootstrap row's token_hash is the empty string, which no
        // token_hash() output can ever equal, so it admits nothing here.
        let row: Option<i64> = sqlx::query_scalar("SELECT 1 FROM gateways WHERE token_hash = ?1")
            .bind(token_hash(token))
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    async fn upsert_fleet_identity(
        &self,
        batch: &CameraTelemetryBatch,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO organizations (id, name) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
        )
        .bind(&batch.customer_id)
        .bind(&batch.customer_name)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO sites (id, org_id, name, city) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET org_id = excluded.org_id,
                 name = excluded.name, city = excluded.city",
        )
        .bind(&batch.site_id)
        .bind(&batch.customer_id)
        .bind(&batch.site_name)
        .bind(&batch.city)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO gateways (id, site_id, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET site_id = excluded.site_id,
                 last_seen = excluded.last_seen",
        )
        .bind(&batch.gateway_id)
        .bind(&batch.site_id)
        .bind(ts(&now))
        .execute(&mut *tx)
        .await?;
        for camera in &batch.cameras {
            sqlx::query(
                "INSERT INTO cameras (id, gateway_id, site_id, name, manufacturer, model,
                     firmware, codec, width, height, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
                 ON CONFLICT(id) DO UPDATE SET gateway_id = excluded.gateway_id,
                     site_id = excluded.site_id, name = excluded.name,
                     manufacturer = excluded.manufacturer, model = excluded.model,
                     firmware = excluded.firmware, codec = excluded.codec,
                     width = excluded.width, height = excluded.height,
                     last_seen = excluded.last_seen, retired_at = NULL",
            )
            .bind(&camera.camera_id)
            .bind(&batch.gateway_id)
            .bind(&camera.site_id)
            .bind(&camera.name)
            .bind(&camera.manufacturer)
            .bind(&camera.model)
            .bind(&camera.firmware)
            .bind(&camera.codec)
            .bind(camera.width.map(i64::from))
            .bind(camera.height.map(i64::from))
            .bind(ts(&now))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn fleet_cameras(&self) -> Result<Vec<CameraRecord>, StoreError> {
        let rows = sqlx::query("SELECT * FROM cameras WHERE retired_at IS NULL ORDER BY name, id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(camera_from_row).collect()
    }

    async fn fleet_identity(&self) -> Result<Vec<OrganizationRecord>, StoreError> {
        let org_rows = sqlx::query("SELECT id, name FROM organizations ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        let site_rows = sqlx::query("SELECT id, org_id, name, city FROM sites ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        let camera_rows =
            sqlx::query("SELECT * FROM cameras WHERE retired_at IS NULL ORDER BY name, id")
                .fetch_all(&self.pool)
                .await?;

        let mut cameras_by_site: std::collections::HashMap<String, Vec<CameraRecord>> =
            std::collections::HashMap::new();
        for row in &camera_rows {
            let camera = camera_from_row(row)?;
            cameras_by_site
                .entry(camera.site_id.clone())
                .or_default()
                .push(camera);
        }

        let mut sites_by_org: std::collections::HashMap<String, Vec<SiteRecord>> =
            std::collections::HashMap::new();
        for row in &site_rows {
            let site = SiteRecord {
                id: row.get("id"),
                org_id: row.get("org_id"),
                name: row.get("name"),
                city: row.get("city"),
                cameras: cameras_by_site
                    .remove::<str>(row.get("id"))
                    .unwrap_or_default(),
            };
            sites_by_org
                .entry(site.org_id.clone())
                .or_default()
                .push(site);
        }

        Ok(org_rows
            .iter()
            .map(|row| OrganizationRecord {
                id: row.get("id"),
                name: row.get("name"),
                sites: sites_by_org
                    .remove::<str>(row.get("id"))
                    .unwrap_or_default(),
            })
            .collect())
    }

    async fn save_recording(&self, manifest: &RecordingManifest) -> Result<(), StoreError> {
        let json =
            serde_json::to_string(manifest).map_err(|err| StoreError::Internal(err.into()))?;
        sqlx::query(
            "INSERT INTO recordings (id, camera_id, started_at, ended_at, delete_after, codec, manifest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET camera_id = excluded.camera_id,
                 started_at = excluded.started_at, ended_at = excluded.ended_at,
                 delete_after = excluded.delete_after, codec = excluded.codec,
                 manifest = excluded.manifest",
        )
        .bind(&manifest.recording_id)
        .bind(&manifest.camera_id)
        .bind(ts(&manifest.started_at))
        .bind(ts(&manifest.ended_at))
        .bind(manifest.delete_after.as_ref().map(ts))
        .bind(&manifest.codec)
        .bind(json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn recording(&self, recording_id: &str) -> Result<RecordingManifest, StoreError> {
        let row = sqlx::query("SELECT manifest FROM recordings WHERE id = ?1")
            .bind(recording_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        manifest_from_row(&row)
    }

    async fn camera_recordings(
        &self,
        camera_id: &str,
    ) -> Result<Vec<RecordingManifest>, StoreError> {
        let rows = sqlx::query(
            "SELECT manifest FROM recordings WHERE camera_id = ?1 ORDER BY started_at DESC",
        )
        .bind(camera_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(manifest_from_row).collect()
    }

    async fn expired_recordings(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<RecordingManifest>, StoreError> {
        let rows = sqlx::query(
            "SELECT manifest FROM recordings
             WHERE delete_after IS NOT NULL AND delete_after <= ?1",
        )
        .bind(ts(&now))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(manifest_from_row).collect()
    }

    async fn delete_recording(&self, recording_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM recordings WHERE id = ?1")
            .bind(recording_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn admin_password_hash(&self) -> Result<Option<String>, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT password_hash FROM admin_credential WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn set_admin_password_hash(
        &self,
        phc: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO admin_credential (id, password_hash, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET
                 password_hash = excluded.password_hash,
                 updated_at = excluded.updated_at",
        )
        .bind(phc)
        .bind(ts(&now))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn create_session(
        &self,
        session_id: &str,
        user_id: &str,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO sessions (id_hash, created_at, expires_at, user_id)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(token_hash(session_id))
        .bind(ts(&now))
        .bind(ts(&expires_at))
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn session_user(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<SessionUser>, StoreError> {
        let row = sqlx::query(
            "SELECT u.id AS id, u.email AS email, u.role AS role, u.customer_id AS customer_id
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.id_hash = ?1 AND s.expires_at > ?2 AND u.disabled_at IS NULL",
        )
        .bind(token_hash(session_id))
        .bind(ts(&now))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let role: String = row.try_get("role")?;
        Ok(Some(SessionUser {
            id: row.try_get("id")?,
            email: row.try_get("email")?,
            // An unknown role reads as the least of them: a newer control
            // plane's "superuser" must not become one here.
            role: Role::parse(&role).unwrap_or(Role::Viewer),
            customer_id: row.try_get("customer_id")?,
        }))
    }

    async fn create_user(
        &self,
        id: &str,
        email: &str,
        password_hash: &str,
        role: Role,
        customer_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let inserted = sqlx::query(
            "INSERT INTO users (id, email, password_hash, role, customer_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(email) DO NOTHING",
        )
        .bind(id)
        .bind(email)
        .bind(password_hash)
        .bind(role.as_str())
        .bind(customer_id)
        .bind(ts(&now))
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::AlreadyExists);
        }
        Ok(())
    }

    async fn user_by_email(&self, email: &str) -> Result<Option<StoredUser>, StoreError> {
        let row = sqlx::query("SELECT * FROM users WHERE email = ?1")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let role: String = row.try_get("role")?;
        Ok(Some(StoredUser {
            id: row.try_get("id")?,
            email: row.try_get("email")?,
            password_hash: row.try_get("password_hash")?,
            role: Role::parse(&role).unwrap_or(Role::Viewer),
            disabled: row.try_get::<Option<String>, _>("disabled_at")?.is_some(),
        }))
    }

    async fn users(&self) -> Result<Vec<UserView>, StoreError> {
        let rows = sqlx::query("SELECT * FROM users ORDER BY email")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let role: String = row.try_get("role")?;
                Ok(UserView {
                    id: row.try_get("id")?,
                    email: row.try_get("email")?,
                    role: Role::parse(&role).unwrap_or(Role::Viewer),
                    customer_id: row.try_get("customer_id")?,
                    created_at: parse_ts(row.try_get("created_at")?)?,
                    disabled_at: row
                        .try_get::<Option<String>, _>("disabled_at")?
                        .as_deref()
                        .map(parse_ts)
                        .transpose()?,
                })
            })
            .collect()
    }

    async fn update_user(
        &self,
        id: &str,
        role: Option<Role>,
        password_hash: Option<&str>,
        disabled: Option<bool>,
        customer_id: Option<Option<&str>>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut changed = false;
        if let Some(role) = role {
            sqlx::query("UPDATE users SET role = ?2 WHERE id = ?1")
                .bind(id)
                .bind(role.as_str())
                .execute(&self.pool)
                .await?;
            changed = true;
        }
        if let Some(hash) = password_hash {
            sqlx::query("UPDATE users SET password_hash = ?2 WHERE id = ?1")
                .bind(id)
                .bind(hash)
                .execute(&self.pool)
                .await?;
            changed = true;
        }
        if let Some(disabled) = disabled {
            sqlx::query("UPDATE users SET disabled_at = ?2 WHERE id = ?1")
                .bind(id)
                .bind(disabled.then(|| ts(&now)))
                .execute(&self.pool)
                .await?;
            changed = true;
        }
        if let Some(customer_id) = customer_id {
            sqlx::query("UPDATE users SET customer_id = ?2 WHERE id = ?1")
                .bind(id)
                .bind(customer_id)
                .execute(&self.pool)
                .await?;
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        // Prove the user existed, rather than reporting success for a typo.
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = ?1")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        if exists == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn delete_sessions_of(&self, user_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE user_id = ?1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE id_hash = ?1")
            .bind(token_hash(session_id))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_all_sessions(&self) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_expired_sessions(&self, now: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE expires_at <= ?1")
            .bind(ts(&now))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn open_incident(
        &self,
        camera_id: &str,
        started_at: DateTime<Utc>,
        detail: Option<&str>,
    ) -> Result<bool, StoreError> {
        let inserted = sqlx::query(
            "INSERT INTO incidents (id, camera_id, started_at, ended_at, detail)
             VALUES (?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(camera_id) WHERE ended_at IS NULL DO NOTHING",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(camera_id)
        .bind(ts(&started_at))
        .bind(detail)
        .execute(&self.pool)
        .await?;
        // Nothing inserted means this camera was already down: the same
        // outage, reported again, not a new one.
        Ok(inserted.rows_affected() > 0)
    }

    async fn close_incident(
        &self,
        camera_id: &str,
        ended_at: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let closed = sqlx::query(
            "UPDATE incidents SET ended_at = ?2 WHERE camera_id = ?1 AND ended_at IS NULL",
        )
        .bind(camera_id)
        .bind(ts(&ended_at))
        .execute(&self.pool)
        .await?;
        // The reconciler closes unconditionally every pass, so most calls
        // close nothing. One that does is a camera coming back.
        Ok(closed.rows_affected() > 0)
    }

    async fn incidents(&self, limit: i64) -> Result<Vec<IncidentView>, StoreError> {
        let rows = sqlx::query(
            "SELECT i.camera_id,
                    COALESCE(c.name, i.camera_id) AS camera_name,
                    COALESCE(c.site_id, '') AS site_id,
                    COALESCE(s.name, '') AS site_name,
                    i.started_at, i.ended_at, i.detail
             FROM incidents i
             LEFT JOIN cameras c ON c.id = i.camera_id
             LEFT JOIN sites s ON s.id = c.site_id
             ORDER BY (i.ended_at IS NULL) DESC, i.started_at DESC
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(IncidentView {
                    camera_id: row.get("camera_id"),
                    camera_name: row.get("camera_name"),
                    site_id: row.get("site_id"),
                    site_name: row.get("site_name"),
                    started_at: parse_ts(row.get::<String, _>("started_at").as_str())?,
                    ended_at: row
                        .get::<Option<String>, _>("ended_at")
                        .map(|value| parse_ts(&value))
                        .transpose()?,
                    detail: row.get("detail"),
                })
            })
            .collect()
    }

    async fn delete_closed_incidents_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM incidents WHERE ended_at IS NOT NULL AND ended_at < ?1")
            .bind(ts(&cutoff))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn revoke_gateway(&self, gateway_id: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        let result =
            sqlx::query("UPDATE gateways SET revoked_at = ?2, token_hash = '' WHERE id = ?1")
                .bind(gateway_id)
                .bind(ts(&now))
                .execute(&self.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn add_video_source(&self, source: &VideoSource) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO video_sources (id, gateway_id, name, kind, address, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&source.id)
        .bind(&source.gateway_id)
        .bind(&source.name)
        .bind(source.kind.as_str())
        .bind(&source.address)
        .bind(ts(&source.added_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn video_sources(&self) -> Result<Vec<VideoSource>, StoreError> {
        let rows = sqlx::query("SELECT * FROM video_sources ORDER BY added_at, id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(source_from_row).collect()
    }

    async fn gateway_video_sources(
        &self,
        gateway_id: &str,
    ) -> Result<Vec<VideoSource>, StoreError> {
        let rows =
            sqlx::query("SELECT * FROM video_sources WHERE gateway_id = ?1 ORDER BY added_at, id")
                .bind(gateway_id)
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(source_from_row).collect()
    }

    async fn set_recording_policy(&self, policy: &RecordingPolicy) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO recording_policies
                 (camera_id, gateway_id, mode, keep, retention_days, storage_plugin_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(camera_id) DO UPDATE SET
                 gateway_id = excluded.gateway_id, mode = excluded.mode, keep = excluded.keep,
                 retention_days = excluded.retention_days,
                 storage_plugin_id = excluded.storage_plugin_id,
                 updated_at = excluded.updated_at",
        )
        .bind(&policy.camera_id)
        .bind(&policy.gateway_id)
        .bind(policy.mode.as_str())
        .bind(serde_json::to_string(&policy.keep).map_err(|error| {
            StoreError::Internal(anyhow::anyhow!("keep rules will not serialise: {error}"))
        })?)
        .bind(i64::from(policy.retention_days))
        .bind(&policy.storage_plugin_id)
        .bind(ts(&policy.updated_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn recording_policy(
        &self,
        camera_id: &str,
    ) -> Result<Option<RecordingPolicy>, StoreError> {
        let row = sqlx::query("SELECT * FROM recording_policies WHERE camera_id = ?1")
            .bind(camera_id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(policy_from_row).transpose()
    }

    async fn gateway_recording_policies(
        &self,
        gateway_id: &str,
    ) -> Result<Vec<RecordingPolicy>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM recording_policies WHERE gateway_id = ?1 ORDER BY camera_id",
        )
        .bind(gateway_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(policy_from_row).collect()
    }

    async fn fold_health(&self, samples: &[crate::health::HealthSample]) -> Result<(), StoreError> {
        if samples.is_empty() {
            return Ok(());
        }
        let mut transaction = self.pool.begin().await?;
        for sample in samples {
            let (healthy, warning, offline) = match sample.status {
                vms_domain::HealthStatus::Healthy => (sample.seconds, 0, 0),
                vms_domain::HealthStatus::Warning => (0, sample.seconds, 0),
                vms_domain::HealthStatus::Offline => (0, 0, sample.seconds),
            };
            // An average needs a count, and a camera that reports no rate
            // must not drag one down: only a sample that carried a number is
            // counted towards it.
            let rated = i64::from(sample.fps.is_some() || sample.bitrate_kbps.is_some());
            sqlx::query(
                "INSERT INTO camera_health_hours
                     (camera_id, hour, healthy_seconds, warning_seconds, offline_seconds,
                      reconnects, fps_total, bitrate_total, samples, worst_loss)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(camera_id, hour) DO UPDATE SET
                     healthy_seconds = healthy_seconds + excluded.healthy_seconds,
                     warning_seconds = warning_seconds + excluded.warning_seconds,
                     offline_seconds = offline_seconds + excluded.offline_seconds,
                     reconnects = reconnects + excluded.reconnects,
                     fps_total = fps_total + excluded.fps_total,
                     bitrate_total = bitrate_total + excluded.bitrate_total,
                     samples = samples + excluded.samples,
                     worst_loss = MAX(worst_loss, excluded.worst_loss)",
            )
            .bind(&sample.camera_id)
            .bind(ts(&sample.hour))
            .bind(healthy)
            .bind(warning)
            .bind(offline)
            .bind(sample.reconnects)
            .bind(f64::from(sample.fps.unwrap_or(0.0)))
            .bind(f64::from(sample.bitrate_kbps.unwrap_or(0)))
            .bind(rated)
            .bind(i64::try_from(sample.packet_loss).unwrap_or(i64::MAX))
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn camera_health(
        &self,
        camera_id: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<HealthHour>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM camera_health_hours WHERE camera_id = ?1 AND hour >= ?2 ORDER BY hour",
        )
        .bind(camera_id)
        .bind(ts(&since))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(health_from_row).collect()
    }

    async fn fleet_health(&self, since: DateTime<Utc>) -> Result<Vec<HealthHour>, StoreError> {
        let rows = sqlx::query(
            "SELECT hour,
                    SUM(healthy_seconds) AS healthy_seconds,
                    SUM(warning_seconds) AS warning_seconds,
                    SUM(offline_seconds) AS offline_seconds,
                    SUM(reconnects) AS reconnects,
                    SUM(fps_total) AS fps_total,
                    SUM(bitrate_total) AS bitrate_total,
                    SUM(samples) AS samples,
                    MAX(worst_loss) AS worst_loss
             FROM camera_health_hours WHERE hour >= ?1 GROUP BY hour ORDER BY hour",
        )
        .bind(ts(&since))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(health_from_row).collect()
    }

    async fn delete_health_before(&self, cutoff: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM camera_health_hours WHERE hour < ?1")
            .bind(ts(&cutoff))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn plugin_registrations(&self) -> Result<Vec<StoredRegistration>, StoreError> {
        let rows = sqlx::query("SELECT * FROM plugin_registrations ORDER BY plugin_id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(StoredRegistration {
                    plugin_id: row.try_get("plugin_id")?,
                    endpoint: row.try_get("endpoint")?,
                    placement: row.try_get("placement")?,
                    enabled: row.try_get::<i64, _>("enabled")? != 0,
                    token_env: row.try_get("token_env")?,
                    token_file: row.try_get("token_file")?,
                    customer_id: row.try_get("customer_id")?,
                })
            })
            .collect()
    }

    async fn save_plugin_registration(
        &self,
        registration: &StoredRegistration,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO plugin_registrations
                 (plugin_id, endpoint, placement, enabled, token_env, token_file,
                  customer_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(plugin_id) DO UPDATE SET
                 endpoint = excluded.endpoint, placement = excluded.placement,
                 enabled = excluded.enabled, token_env = excluded.token_env,
                 token_file = excluded.token_file, customer_id = excluded.customer_id",
        )
        .bind(&registration.plugin_id)
        .bind(&registration.endpoint)
        .bind(&registration.placement)
        .bind(i64::from(registration.enabled))
        .bind(&registration.token_env)
        .bind(&registration.token_file)
        .bind(&registration.customer_id)
        .bind(ts(&now))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_plugin_registration(&self, plugin_id: &str) -> Result<(), StoreError> {
        let removed = sqlx::query("DELETE FROM plugin_registrations WHERE plugin_id = ?1")
            .bind(plugin_id)
            .execute(&self.pool)
            .await?;
        if removed.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn record_event(
        &self,
        event: &FleetEvent,
        sinks: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO fleet_events
                 (id, kind, severity, occurred_at, customer_id, site_id, site_name,
                  gateway_id, camera_id, title, detail, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(&event.id)
        .bind(event.kind.as_str())
        .bind(event.severity.as_str())
        .bind(ts(&event.occurred_at))
        .bind(&event.customer_id)
        .bind(&event.site_id)
        .bind(&event.site_name)
        .bind(&event.gateway_id)
        .bind(&event.camera_id)
        .bind(&event.title)
        .bind(&event.detail)
        .bind(serde_json::to_string(&event.metadata).unwrap_or_else(|_| "{}".into()))
        .execute(&mut *transaction)
        .await?;
        for plugin_id in sinks {
            sqlx::query(
                "INSERT INTO event_deliveries (event_id, plugin_id, attempts, next_attempt_at)
                 VALUES (?1, ?2, 0, ?3)
                 ON CONFLICT(event_id, plugin_id) DO NOTHING",
            )
            .bind(&event.id)
            .bind(plugin_id)
            .bind(ts(&now))
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn due_deliveries(
        &self,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<DueDelivery>, StoreError> {
        let rows = sqlx::query(
            "SELECT d.plugin_id AS plugin_id, d.attempts AS attempts, e.*
             FROM event_deliveries d JOIN fleet_events e ON e.id = d.event_id
             WHERE d.delivered_at IS NULL AND d.next_attempt_at IS NOT NULL
                   AND d.next_attempt_at <= ?1
             ORDER BY e.occurred_at, d.plugin_id
             LIMIT ?2",
        )
        .bind(ts(&now))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(DueDelivery {
                    event: event_from_row(row)?,
                    plugin_id: row.try_get("plugin_id")?,
                    attempts: row.try_get("attempts")?,
                })
            })
            .collect()
    }

    async fn delivery_succeeded(
        &self,
        event_id: &str,
        plugin_id: &str,
        declined: bool,
        at: DateTime<Utc>,
        detail: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE event_deliveries
             SET delivered_at = ?3, declined = ?4, last_error = ?5,
                 next_attempt_at = NULL, attempts = attempts + 1
             WHERE event_id = ?1 AND plugin_id = ?2",
        )
        .bind(event_id)
        .bind(plugin_id)
        .bind(ts(&at))
        .bind(i64::from(declined))
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delivery_failed(
        &self,
        event_id: &str,
        plugin_id: &str,
        error: &str,
        next_attempt_at: Option<DateTime<Utc>>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE event_deliveries
             SET attempts = attempts + 1, last_error = ?3, next_attempt_at = ?4
             WHERE event_id = ?1 AND plugin_id = ?2",
        )
        .bind(event_id)
        .bind(plugin_id)
        .bind(error)
        .bind(next_attempt_at.as_ref().map(ts))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn recent_events(&self, limit: i64) -> Result<Vec<EventView>, StoreError> {
        let rows = sqlx::query("SELECT * FROM fleet_events ORDER BY occurred_at DESC, id LIMIT ?1")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        let mut views = Vec::with_capacity(rows.len());
        for row in &rows {
            let event = event_from_row(row)?;
            let deliveries = sqlx::query(
                "SELECT * FROM event_deliveries WHERE event_id = ?1 ORDER BY plugin_id",
            )
            .bind(&event.id)
            .fetch_all(&self.pool)
            .await?;
            let deliveries = deliveries
                .iter()
                .map(|row| {
                    Ok(DeliveryView {
                        plugin_id: row.try_get("plugin_id")?,
                        attempts: row.try_get("attempts")?,
                        delivered_at: row
                            .try_get::<Option<String>, _>("delivered_at")?
                            .as_deref()
                            .map(parse_ts)
                            .transpose()?,
                        declined: row.try_get::<i64, _>("declined")? != 0,
                        last_error: row.try_get("last_error")?,
                        next_attempt_at: row
                            .try_get::<Option<String>, _>("next_attempt_at")?
                            .as_deref()
                            .map(parse_ts)
                            .transpose()?,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            views.push(EventView { event, deliveries });
        }
        Ok(views)
    }

    async fn delete_events_before(&self, cutoff: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM event_deliveries WHERE event_id IN (SELECT id FROM fleet_events WHERE occurred_at < ?1)")
            .bind(ts(&cutoff))
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM fleet_events WHERE occurred_at < ?1")
            .bind(ts(&cutoff))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_video_source(&self, id: &str) -> Result<(), StoreError> {
        let removed = sqlx::query("DELETE FROM video_sources WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if removed.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn retire_gateway_cameras(
        &self,
        gateway_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        // RETURNING, so the caller knows exactly which cameras it just retired
        // and can close their incidents without asking again.
        let rows = sqlx::query(
            "UPDATE cameras SET retired_at = ?2
             WHERE gateway_id = ?1 AND retired_at IS NULL
             RETURNING id",
        )
        .bind(gateway_id)
        .bind(ts(&now))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| Ok(row.try_get("id")?)).collect()
    }

    async fn gateway_exists(&self, gateway_id: &str) -> Result<bool, StoreError> {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM gateways WHERE id = ?1")
            .bind(gateway_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
    }

    async fn gateway_revoked(&self, gateway_id: &str) -> Result<bool, StoreError> {
        let revoked: Option<Option<String>> =
            sqlx::query_scalar("SELECT revoked_at FROM gateways WHERE id = ?1")
                .bind(gateway_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(revoked.flatten().is_some())
    }

    async fn gateway_views(&self) -> Result<Vec<GatewayView>, StoreError> {
        let rows = sqlx::query(
            "SELECT g.id, g.site_id,
                    COALESCE(s.name, '') AS site_name,
                    COALESCE(o.name, '') AS customer_name,
                    g.hostname, g.version,
                    (g.token_hash != '') AS enrolled,
                    g.revoked_at, g.last_seen
             FROM gateways g
             LEFT JOIN sites s ON s.id = g.site_id
             LEFT JOIN organizations o ON o.id = s.org_id
             ORDER BY g.id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(GatewayView {
                    gateway_id: row.get("id"),
                    site_id: row.get("site_id"),
                    site_name: row.get("site_name"),
                    customer_name: row.get("customer_name"),
                    hostname: row.get("hostname"),
                    version: row.get("version"),
                    enrolled: row.get::<i64, _>("enrolled") != 0,
                    revoked_at: row
                        .get::<Option<String>, _>("revoked_at")
                        .map(|v| parse_ts(&v))
                        .transpose()?,
                    last_seen: row
                        .get::<Option<String>, _>("last_seen")
                        .map(|v| parse_ts(&v))
                        .transpose()?,
                    online: false,
                    heartbeat: None,
                })
            })
            .collect()
    }

    async fn record_audit(
        &self,
        at: DateTime<Utc>,
        actor: &str,
        action: &str,
        subject: &str,
        detail: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO audit_log (id, at, actor, action, subject, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(ts(&at))
        .bind(actor)
        .bind(action)
        .bind(subject)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn audit_entries(&self, limit: i64) -> Result<Vec<AuditView>, StoreError> {
        let rows = sqlx::query(
            "SELECT at, actor, action, subject, detail FROM audit_log
             ORDER BY at DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(AuditView {
                    at: parse_ts(row.get::<String, _>("at").as_str())?,
                    actor: row.get("actor"),
                    action: row.get("action"),
                    subject: row.get("subject"),
                    detail: row.get("detail"),
                })
            })
            .collect()
    }

    async fn delete_audit_before(&self, cutoff: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM audit_log WHERE at < ?1")
            .bind(ts(&cutoff))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn health_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<HealthHour, StoreError> {
    let samples: i64 = row.try_get("samples")?;
    let fps_total: f64 = row.try_get("fps_total")?;
    let bitrate_total: f64 = row.try_get("bitrate_total")?;
    Ok(HealthHour {
        hour: parse_ts(row.try_get("hour")?)?,
        healthy_seconds: row.try_get("healthy_seconds")?,
        warning_seconds: row.try_get("warning_seconds")?,
        offline_seconds: row.try_get("offline_seconds")?,
        reconnects: row.try_get("reconnects")?,
        average_fps: (samples > 0).then(|| (fps_total / samples as f64) as f32),
        average_bitrate_kbps: (samples > 0)
            .then(|| (bitrate_total / samples as f64).round() as u32),
        worst_loss: row.try_get("worst_loss")?,
    })
}

fn event_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<FleetEvent, StoreError> {
    let kind: String = row.try_get("kind")?;
    let kind = match kind.as_str() {
        "camera_offline" => FleetEventKind::CameraOffline,
        "camera_recovered" => FleetEventKind::CameraRecovered,
        "gateway_offline" => FleetEventKind::GatewayOffline,
        "gateway_recovered" => FleetEventKind::GatewayRecovered,
        "test" => FleetEventKind::Test,
        other => {
            return Err(StoreError::Internal(anyhow::anyhow!(
                "stored event kind {other:?} is not one this build knows"
            )));
        }
    };
    let severity: String = row.try_get("severity")?;
    let severity = match severity.as_str() {
        "critical" => EventSeverity::Critical,
        "warning" => EventSeverity::Warning,
        _ => EventSeverity::Info,
    };
    let metadata: String = row.try_get("metadata")?;
    Ok(FleetEvent {
        id: row.try_get("id")?,
        kind,
        severity,
        occurred_at: parse_ts(row.try_get("occurred_at")?)?,
        customer_id: row.try_get("customer_id")?,
        site_id: row.try_get("site_id")?,
        site_name: row.try_get("site_name")?,
        gateway_id: row.try_get("gateway_id")?,
        camera_id: row.try_get("camera_id")?,
        title: row.try_get("title")?,
        detail: row.try_get("detail")?,
        metadata: serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null),
    })
}

fn policy_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RecordingPolicy, StoreError> {
    let mode: String = row.try_get("mode")?;
    let mode = match mode.as_str() {
        "continuous" => vms_domain::RecordingMode::Continuous,
        // An unknown mode means a newer control plane wrote it. Off is the
        // reading that records nothing by surprise.
        _ => vms_domain::RecordingMode::Off,
    };
    let keep: String = row.try_get("keep")?;
    let keep = serde_json::from_str(&keep).map_err(|error| {
        StoreError::Internal(anyhow::anyhow!(
            "stored keep rules are not readable: {error}"
        ))
    })?;
    Ok(RecordingPolicy {
        camera_id: row.try_get("camera_id")?,
        gateway_id: row.try_get("gateway_id")?,
        mode,
        keep,
        retention_days: row.try_get::<i64, _>("retention_days")?.clamp(0, 3650) as u16,
        storage_plugin_id: row.try_get("storage_plugin_id")?,
        updated_at: parse_ts(row.try_get("updated_at")?)?,
    })
}

fn source_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<VideoSource, StoreError> {
    let kind: String = row.try_get("kind")?;
    let kind = match kind.as_str() {
        "rtsp" => vms_domain::SourceKind::Rtsp,
        "rtmp" => vms_domain::SourceKind::Rtmp,
        "srt" => vms_domain::SourceKind::Srt,
        other => {
            return Err(StoreError::Internal(anyhow::anyhow!(
                "stored source kind {other:?} is not one this build knows"
            )));
        }
    };
    Ok(VideoSource {
        id: row.try_get("id")?,
        gateway_id: row.try_get("gateway_id")?,
        name: row.try_get("name")?,
        kind,
        address: row.try_get("address")?,
        added_at: parse_ts(row.try_get("added_at")?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, StoreError, token_hash};
    use chrono::{Duration, Utc};
    use vms_domain::{
        CameraTelemetry, CameraTelemetryBatch, EnrollmentRequest, GatewayEnrollmentRequest,
        HealthStatus, RecordingManifest, RecordingObject, VideoSource,
    };

    fn manifest(
        id: &str,
        camera_id: &str,
        delete_after: Option<chrono::DateTime<Utc>>,
    ) -> RecordingManifest {
        let now = Utc::now();
        RecordingManifest {
            recording_id: id.into(),
            camera_id: camera_id.into(),
            gateway_id: "gw-1".into(),
            started_at: now - Duration::seconds(10),
            ended_at: now,
            codec: "avc1.640028".into(),
            width: 1920,
            height: 1080,
            init: RecordingObject {
                storage_plugin_id: "storage-s3".into(),
                object_ref: format!("{id}/init.mp4"),
                object_key: format!("{id}/init.mp4"),
                content_type: "video/mp4".into(),
                size_bytes: 1024,
            },
            segments: vec![],
            delete_after,
        }
    }

    #[tokio::test]
    async fn a_saved_recording_survives_a_restart() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let url = format!("sqlite:{}", file.path().display());
        {
            let store = SqliteStore::connect(&url).await.unwrap();
            store
                .save_recording(&manifest("rec-1", "cam-1", None))
                .await
                .unwrap();
        }
        let reopened = SqliteStore::connect(&url).await.unwrap();
        let loaded = reopened
            .recording("rec-1")
            .await
            .expect("recording survives");
        assert_eq!(loaded.init.object_ref, "rec-1/init.mp4");
        assert!(matches!(
            reopened.recording("rec-none").await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn the_timeline_is_per_camera_and_newest_first() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut older = manifest("rec-old", "cam-1", None);
        older.started_at = Utc::now() - Duration::hours(2);
        store.save_recording(&older).await.unwrap();
        store
            .save_recording(&manifest("rec-new", "cam-1", None))
            .await
            .unwrap();
        store
            .save_recording(&manifest("rec-other", "cam-2", None))
            .await
            .unwrap();
        let timeline = store.camera_recordings("cam-1").await.unwrap();
        let ids: Vec<_> = timeline.iter().map(|r| r.recording_id.as_str()).collect();
        assert_eq!(ids, vec!["rec-new", "rec-old"]);
    }

    #[tokio::test]
    async fn expiry_returns_exactly_the_overdue_manifests() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .save_recording(&manifest(
                "rec-overdue",
                "cam-1",
                Some(now - Duration::minutes(1)),
            ))
            .await
            .unwrap();
        store
            .save_recording(&manifest(
                "rec-later",
                "cam-1",
                Some(now + Duration::hours(1)),
            ))
            .await
            .unwrap();
        store
            .save_recording(&manifest("rec-keep-forever", "cam-1", None))
            .await
            .unwrap();
        let expired = store.expired_recordings(now).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].recording_id, "rec-overdue");
        store.delete_recording("rec-overdue").await.unwrap();
        assert!(store.expired_recordings(now).await.unwrap().is_empty());
        assert!(matches!(
            store.recording("rec-overdue").await,
            Err(StoreError::NotFound)
        ));
    }

    fn batch_with_camera(gateway_id: &str, camera_id: &str, name: &str) -> CameraTelemetryBatch {
        CameraTelemetryBatch {
            gateway_id: gateway_id.into(),
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Barcelona".into(),
            sent_at: Utc::now(),
            cameras: vec![CameraTelemetry {
                camera_id: camera_id.into(),
                gateway_id: gateway_id.into(),
                site_id: "site-1".into(),
                name: name.into(),
                status: HealthStatus::Healthy,
                manufacturer: Some("Dahua".into()),
                model: None,
                firmware: None,
                profile_name: None,
                codec: Some("h264".into()),
                width: Some(1920),
                height: Some(1080),
                fps: Some(25.0),
                bitrate_kbps: Some(1800),
                packet_loss: 0,
                reconnects: 0,
                rtsp_endpoint: Some("rtsp://192.0.2.1/stream".into()),
                last_seen: Utc::now(),
                last_error: None,
            }],
        }
    }

    fn source(id: &str, gateway_id: &str, address: &str) -> VideoSource {
        VideoSource {
            id: id.into(),
            gateway_id: gateway_id.into(),
            name: format!("Source {id}"),
            kind: vms_domain::SourceKind::Rtsp,
            address: address.into(),
            added_at: Utc::now(),
        }
    }

    /// A gateway is told its own sources and nobody else's: the poll is how a
    /// gateway learns what to carry, and one site's addresses are not another's.
    #[tokio::test]
    async fn sources_are_listed_whole_and_per_gateway() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        for gateway in ["gw-1", "gw-2"] {
            store
                .upsert_fleet_identity(
                    &batch_with_camera(gateway, &format!("cam-{gateway}"), "c"),
                    now,
                )
                .await
                .unwrap();
        }
        store
            .add_video_source(&source("src-1", "gw-1", "rtsp://10.0.0.1/stream"))
            .await
            .unwrap();
        store
            .add_video_source(&source("src-2", "gw-1", "rtsp://10.0.0.2/stream"))
            .await
            .unwrap();
        store
            .add_video_source(&source("src-3", "gw-2", "rtsp://10.0.0.3/stream"))
            .await
            .unwrap();

        let all = store.video_sources().await.unwrap();
        assert_eq!(all.len(), 3, "the dashboard sees every source");
        let mine: Vec<String> = store
            .gateway_video_sources("gw-1")
            .await
            .unwrap()
            .into_iter()
            .map(|source| source.id)
            .collect();
        assert_eq!(mine, vec!["src-1".to_string(), "src-2".to_string()]);

        store.delete_video_source("src-1").await.unwrap();
        assert_eq!(store.gateway_video_sources("gw-1").await.unwrap().len(), 1);
        assert!(
            matches!(
                store.delete_video_source("src-1").await,
                Err(StoreError::NotFound)
            ),
            "removing what is not there is a miss, not a silent success"
        );
    }

    #[tokio::test]
    async fn a_source_survives_a_restart_with_its_kind() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let url = format!("sqlite:{}", file.path().display());
        {
            let store = SqliteStore::connect(&url).await.unwrap();
            store
                .upsert_fleet_identity(&batch_with_camera("gw-1", "cam-1", "c"), Utc::now())
                .await
                .unwrap();
            let mut pushed = source("src-push", "gw-1", "yard-entrance");
            pushed.kind = vms_domain::SourceKind::Rtmp;
            store.add_video_source(&pushed).await.unwrap();
        }
        let reopened = SqliteStore::connect(&url).await.unwrap();
        let sources = reopened.video_sources().await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, vms_domain::SourceKind::Rtmp);
        assert_eq!(sources[0].address, "yard-entrance");
    }

    #[tokio::test]
    async fn retiring_a_gateways_cameras_takes_them_out_of_the_roster() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        for (gateway, camera) in [("gw-1", "cam-1"), ("gw-1", "cam-2"), ("gw-2", "cam-3")] {
            store
                .upsert_fleet_identity(&batch_with_camera(gateway, camera, camera), now)
                .await
                .unwrap();
        }

        let mut retired = store.retire_gateway_cameras("gw-1", now).await.unwrap();
        retired.sort();
        assert_eq!(retired, vec!["cam-1".to_string(), "cam-2".to_string()]);

        let left: Vec<_> = store
            .fleet_cameras()
            .await
            .unwrap()
            .into_iter()
            .map(|camera| camera.id)
            .collect();
        assert_eq!(
            left,
            vec!["cam-3".to_string()],
            "only the other gateway's camera stays"
        );
        let in_fleet: usize = store
            .fleet_identity()
            .await
            .unwrap()
            .iter()
            .flat_map(|org| &org.sites)
            .map(|site| site.cameras.len())
            .sum();
        assert_eq!(in_fleet, 1, "the roster the dashboard reads drops them too");

        assert!(
            store
                .retire_gateway_cameras("gw-1", now)
                .await
                .unwrap()
                .is_empty(),
            "a second retire has nothing left to stamp"
        );
    }

    #[tokio::test]
    async fn retiring_a_camera_keeps_its_recordings_reachable() {
        // The roster hides it; the archive does not. A recording whose camera
        // cannot be found would be a recording nobody can play.
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .upsert_fleet_identity(&batch_with_camera("gw-1", "cam-1", "Entrance"), now)
            .await
            .unwrap();
        store
            .save_recording(&manifest("rec-1", "cam-1", None))
            .await
            .unwrap();
        store.retire_gateway_cameras("gw-1", now).await.unwrap();

        assert_eq!(
            store.recording("rec-1").await.unwrap().camera_id,
            "cam-1",
            "playback looks the recording up by id"
        );
        assert_eq!(
            store.camera_recordings("cam-1").await.unwrap().len(),
            1,
            "and the timeline still has it"
        );
    }

    #[tokio::test]
    async fn a_retired_camera_comes_back_when_a_gateway_reports_it() {
        let store = SqliteStore::in_memory().await.unwrap();
        let first = Utc::now();
        store
            .upsert_fleet_identity(&batch_with_camera("gw-old", "cam-1", "Entrance"), first)
            .await
            .unwrap();
        store.retire_gateway_cameras("gw-old", first).await.unwrap();
        assert!(store.fleet_cameras().await.unwrap().is_empty());

        // The replacement gateway at the same site reports the same camera.
        let later = first + Duration::minutes(5);
        store
            .upsert_fleet_identity(&batch_with_camera("gw-new", "cam-1", "Entrance"), later)
            .await
            .unwrap();
        let cameras = store.fleet_cameras().await.unwrap();
        assert_eq!(cameras.len(), 1, "it is back in the roster");
        assert_eq!(cameras[0].gateway_id, "gw-new", "under the new gateway");
        assert_eq!(
            cameras[0].first_seen.timestamp_micros(),
            first.timestamp_micros(),
            "with the history it had"
        );
    }

    #[tokio::test]
    async fn telemetry_creates_identity_and_repeats_preserve_first_seen() {
        let store = SqliteStore::in_memory().await.unwrap();
        let first = Utc::now();
        store
            .upsert_fleet_identity(&batch_with_camera("gw-1", "cam-1", "Entrance"), first)
            .await
            .unwrap();
        let later = first + Duration::minutes(5);
        let mut renamed = batch_with_camera("gw-1", "cam-1", "Front entrance");
        renamed.customer_name = "Customer Renamed".into();
        store.upsert_fleet_identity(&renamed, later).await.unwrap();

        let cameras = store.fleet_cameras().await.unwrap();
        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0].name, "Front entrance");
        assert_eq!(
            cameras[0].first_seen.timestamp_micros(),
            first.timestamp_micros()
        );
        assert_eq!(
            cameras[0].last_seen.timestamp_micros(),
            later.timestamp_micros()
        );

        let orgs = store.fleet_identity().await.unwrap();
        assert_eq!(orgs.len(), 1);
        assert_eq!(orgs[0].name, "Customer Renamed");
        assert_eq!(orgs[0].sites.len(), 1);
        assert_eq!(orgs[0].sites[0].cameras.len(), 1);
    }

    #[tokio::test]
    async fn telemetry_does_not_clobber_an_enrolled_gateways_token() {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .enroll_gateway(&request(), &enroll_req("gw-1"), "gw-1-token", Utc::now())
            .await
            .unwrap();
        store
            .upsert_fleet_identity(&batch_with_camera("gw-1", "cam-1", "Entrance"), Utc::now())
            .await
            .unwrap();
        assert!(
            store
                .verify_gateway_token("gw-1", "gw-1-token")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_bootstrap_gateway_gets_a_row_but_no_usable_token() {
        // Telemetry via the shared GATEWAY_TOKEN may arrive before any enrollment.
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .upsert_fleet_identity(
                &batch_with_camera("gw-boot", "cam-1", "Entrance"),
                Utc::now(),
            )
            .await
            .unwrap();
        assert!(!store.verify_gateway_token("gw-boot", "").await.unwrap());
        assert_eq!(store.fleet_cameras().await.unwrap().len(), 1);
    }

    fn enroll_req(gateway_id: &str) -> GatewayEnrollmentRequest {
        GatewayEnrollmentRequest {
            enrollment_token: "unused-here".into(),
            gateway_id: gateway_id.into(),
            hostname: "edge-1".into(),
            version: "0.1.0".into(),
        }
    }

    #[tokio::test]
    async fn an_enrolled_gateway_survives_a_restart() {
        // The reason this store exists: reopen the same file, the token still works.
        let file = tempfile::NamedTempFile::new().unwrap();
        let url = format!("sqlite:{}", file.path().display());
        {
            let store = SqliteStore::connect(&url).await.unwrap();
            store
                .enroll_gateway(&request(), &enroll_req("gw-1"), "gw-1-token", Utc::now())
                .await
                .unwrap();
            assert!(
                store
                    .verify_gateway_token("gw-1", "gw-1-token")
                    .await
                    .unwrap()
            );
        } // store dropped: the "restart"
        let reopened = SqliteStore::connect(&url).await.unwrap();
        assert!(
            reopened
                .verify_gateway_token("gw-1", "gw-1-token")
                .await
                .unwrap()
        );
        assert!(
            !reopened
                .verify_gateway_token("gw-1", "wrong-token")
                .await
                .unwrap()
        );
        assert!(
            !reopened
                .verify_gateway_token("gw-2", "gw-1-token")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn gateway_tokens_are_not_stored_in_plaintext() {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .enroll_gateway(&request(), &enroll_req("gw-1"), "gw-1-token", Utc::now())
            .await
            .unwrap();
        let stored: String =
            sqlx::query_scalar("SELECT token_hash FROM gateways WHERE id = 'gw-1'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(stored, token_hash("gw-1-token"));
        assert_ne!(stored, "gw-1-token");
    }

    #[tokio::test]
    async fn any_enrolled_gateways_token_is_recognised_without_naming_the_gateway() {
        // The plugin endpoints the edge calls carry no gateway id, so their
        // check is "does this token belong to any enrolled gateway".
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .enroll_gateway(&request(), &enroll_req("gw-1"), "gw-1-token", Utc::now())
            .await
            .unwrap();
        store
            .upsert_fleet_identity(
                &batch_with_camera("gw-boot", "cam-1", "Entrance"),
                Utc::now(),
            )
            .await
            .unwrap();
        assert!(store.verify_any_gateway_token("gw-1-token").await.unwrap());
        assert!(!store.verify_any_gateway_token("wrong-token").await.unwrap());
        // A bootstrap gateway row (empty token_hash) must not admit anything,
        // least of all an empty bearer.
        assert!(!store.verify_any_gateway_token("").await.unwrap());
    }

    #[tokio::test]
    async fn re_enrollment_rotates_the_token() {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .enroll_gateway(&request(), &enroll_req("gw-1"), "old-token", Utc::now())
            .await
            .unwrap();
        store
            .enroll_gateway(&request(), &enroll_req("gw-1"), "new-token", Utc::now())
            .await
            .unwrap();
        assert!(
            !store
                .verify_gateway_token("gw-1", "old-token")
                .await
                .unwrap()
        );
        assert!(
            store
                .verify_gateway_token("gw-1", "new-token")
                .await
                .unwrap()
        );
    }

    fn request() -> EnrollmentRequest {
        EnrollmentRequest {
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Barcelona".into(),
        }
    }

    #[tokio::test]
    async fn an_enrollment_claims_exactly_once() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .create_enrollment("TOKEN-A", &request(), now + Duration::minutes(30))
            .await
            .unwrap();

        let claimed = store
            .claim_enrollment("TOKEN-A", now)
            .await
            .expect("first claim");
        assert_eq!(claimed.customer_id, "cust-1");

        match store.claim_enrollment("TOKEN-A", now).await {
            Err(StoreError::Gone) => {}
            other => panic!("second claim must be Gone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_expired_enrollment_is_gone_and_an_unknown_one_is_not_found() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .create_enrollment("TOKEN-B", &request(), now - Duration::seconds(1))
            .await
            .unwrap();
        assert!(matches!(
            store.claim_enrollment("TOKEN-B", now).await,
            Err(StoreError::Gone)
        ));
        assert!(matches!(
            store.claim_enrollment("NEVER-ISSUED", now).await,
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.enrollment_request("NEVER-ISSUED", now).await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn enrollment_tokens_are_not_stored_in_plaintext() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .create_enrollment("SECRET-TOKEN", &request(), now + Duration::minutes(30))
            .await
            .unwrap();
        let stored: String = sqlx::query_scalar("SELECT token_hash FROM enrollments")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_ne!(stored, "SECRET-TOKEN");
        assert_eq!(stored, token_hash("SECRET-TOKEN"));
        assert_eq!(stored.len(), 64);
    }

    #[tokio::test]
    async fn connect_applies_migrations() {
        let store = SqliteStore::in_memory().await.expect("in-memory store");
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&store.pool)
                .await
                .expect("list tables");
        for expected in [
            "organizations",
            "sites",
            "gateways",
            "enrollments",
            "cameras",
            "recordings",
            "admin_credential",
            "sessions",
            "incidents",
            "audit_log",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}, have {tables:?}"
            );
        }
    }

    #[tokio::test]
    async fn revoking_kills_the_token_and_a_fresh_enrollment_clears_it() {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .enroll_gateway(&request(), &enroll_req("gw-1"), "tok-old", Utc::now())
            .await
            .unwrap();
        assert!(store.verify_gateway_token("gw-1", "tok-old").await.unwrap());
        assert!(!store.gateway_revoked("gw-1").await.unwrap());

        store.revoke_gateway("gw-1", Utc::now()).await.unwrap();
        assert!(store.gateway_revoked("gw-1").await.unwrap());
        assert!(
            !store.verify_gateway_token("gw-1", "tok-old").await.unwrap(),
            "a revoked gateway's token must stop working"
        );
        assert!(
            !store.verify_any_gateway_token("tok-old").await.unwrap(),
            "the cleared hash must not match the any-gateway check either"
        );

        // A fresh admin-issued enrollment is the un-revoke.
        store
            .enroll_gateway(&request(), &enroll_req("gw-1"), "tok-new", Utc::now())
            .await
            .unwrap();
        assert!(!store.gateway_revoked("gw-1").await.unwrap());
        assert!(store.verify_gateway_token("gw-1", "tok-new").await.unwrap());

        // Unknown ids: revoking is NotFound, the check is a calm false.
        assert!(matches!(
            store.revoke_gateway("gw-never", Utc::now()).await,
            Err(StoreError::NotFound)
        ));
        assert!(!store.gateway_revoked("gw-never").await.unwrap());
    }

    #[tokio::test]
    async fn the_gateway_roster_carries_names_flags_and_revocation() {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .enroll_gateway(&request(), &enroll_req("gw-1"), "tok-1", Utc::now())
            .await
            .unwrap();
        store
            .upsert_fleet_identity(
                &batch_with_camera("gw-boot", "cam-1", "Entrance"),
                Utc::now(),
            )
            .await
            .unwrap();
        store.revoke_gateway("gw-1", Utc::now()).await.unwrap();

        let views = store.gateway_views().await.unwrap();
        assert_eq!(views.len(), 2);
        let gw1 = views.iter().find(|v| v.gateway_id == "gw-1").unwrap();
        assert_eq!(gw1.site_name, "Site");
        assert_eq!(gw1.customer_name, "Customer");
        assert!(gw1.revoked_at.is_some());
        assert!(!gw1.enrolled, "a revoked gateway has no working token");
        assert!(
            !gw1.online && gw1.heartbeat.is_none(),
            "the store never claims liveness"
        );
        let boot = views.iter().find(|v| v.gateway_id == "gw-boot").unwrap();
        assert!(!boot.enrolled && boot.revoked_at.is_none());
    }

    #[tokio::test]
    async fn audit_rows_insert_list_newest_first_and_prune_by_cutoff() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .record_audit(
                now - Duration::days(400),
                "system",
                "password.reset",
                "",
                None,
            )
            .await
            .unwrap();
        store
            .record_audit(
                now - Duration::minutes(5),
                "admin",
                "login.failed",
                "",
                None,
            )
            .await
            .unwrap();
        store
            .record_audit(
                now,
                "admin",
                "gateway.revoked",
                "gw-1",
                Some("from the dashboard"),
            )
            .await
            .unwrap();

        let entries = store.audit_entries(10).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].action, "gateway.revoked");
        assert_eq!(entries[0].subject, "gw-1");
        assert_eq!(entries[0].detail.as_deref(), Some("from the dashboard"));
        assert_eq!(entries[2].action, "password.reset");

        store
            .delete_audit_before(now - Duration::days(365))
            .await
            .unwrap();
        let entries = store.audit_entries(10).await.unwrap();
        assert_eq!(entries.len(), 2, "only the 400-day-old row goes");
        assert_eq!(
            store.audit_entries(1).await.unwrap().len(),
            1,
            "the limit caps the page"
        );
    }

    #[tokio::test]
    async fn at_most_one_open_incident_per_camera_and_a_flap_is_two_rows() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .open_incident("cam-1", now, Some("gateway telemetry is stale"))
            .await
            .unwrap();
        store
            .open_incident("cam-1", now + Duration::minutes(1), None)
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM incidents")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "the second open must be a no-op");

        store
            .close_incident("cam-1", now + Duration::minutes(5))
            .await
            .unwrap();
        // Closing nothing is Ok — the reconciler closes unconditionally.
        store
            .close_incident("cam-1", now + Duration::minutes(6))
            .await
            .unwrap();
        store
            .open_incident("cam-1", now + Duration::minutes(10), None)
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM incidents")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows, 2, "a flap is two incidents, not one reopened row");
    }

    #[tokio::test]
    async fn incidents_come_open_first_with_joined_names() {
        let store = SqliteStore::in_memory().await.unwrap();
        store
            .upsert_fleet_identity(&batch_with_camera("gw-1", "cam-1", "Entrance"), Utc::now())
            .await
            .unwrap();
        let now = Utc::now();
        store
            .open_incident("cam-1", now - Duration::hours(2), None)
            .await
            .unwrap();
        store
            .close_incident("cam-1", now - Duration::hours(1))
            .await
            .unwrap();
        store
            .open_incident(
                "cam-1",
                now - Duration::minutes(5),
                Some("rtsp: connection refused"),
            )
            .await
            .unwrap();
        store
            .open_incident("cam-gone", now - Duration::days(3), None)
            .await
            .unwrap();

        let incidents = store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 3);
        assert!(
            incidents[0].ended_at.is_none() && incidents[1].ended_at.is_none(),
            "open incidents come first: {incidents:?}"
        );
        assert_eq!(
            incidents[0].camera_name, "Entrance",
            "names join from the roster"
        );
        assert_eq!(incidents[0].site_name, "Site");
        assert_eq!(
            incidents[0].detail.as_deref(),
            Some("rtsp: connection refused")
        );
        // A camera no longer in the roster still shows, under its id.
        assert_eq!(incidents[1].camera_name, "cam-gone");
        assert!(incidents[2].ended_at.is_some());
    }

    #[tokio::test]
    async fn pruning_removes_only_old_closed_incidents() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .open_incident("cam-old", now - Duration::days(120), None)
            .await
            .unwrap();
        store
            .close_incident("cam-old", now - Duration::days(119))
            .await
            .unwrap();
        store
            .open_incident("cam-recent", now - Duration::days(2), None)
            .await
            .unwrap();
        store
            .close_incident("cam-recent", now - Duration::days(1))
            .await
            .unwrap();
        store
            .open_incident("cam-stuck", now - Duration::days(200), None)
            .await
            .unwrap();

        store
            .delete_closed_incidents_before(now - Duration::days(90))
            .await
            .unwrap();
        let left = store.incidents(10).await.unwrap();
        assert_eq!(left.len(), 2, "only the old closed incident goes: {left:?}");
        assert!(
            left.iter()
                .any(|i| i.camera_id == "cam-stuck" && i.ended_at.is_none()),
            "an open incident is never pruned, however old"
        );
        assert!(left.iter().any(|i| i.camera_id == "cam-recent"));
    }

    #[tokio::test]
    async fn the_admin_credential_is_a_single_replaceable_row() {
        let store = SqliteStore::in_memory().await.unwrap();
        assert_eq!(store.admin_password_hash().await.unwrap(), None);
        store
            .set_admin_password_hash("$argon2id$fake-one", Utc::now())
            .await
            .unwrap();
        store
            .set_admin_password_hash("$argon2id$fake-two", Utc::now())
            .await
            .unwrap();
        assert_eq!(
            store.admin_password_hash().await.unwrap().as_deref(),
            Some("$argon2id$fake-two")
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_credential")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "the second seed must replace, not accumulate");
    }

    /// A user to hang sessions on.
    async fn a_user(store: &SqliteStore, email: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create_user(&id, email, "$argon2id$fake", Role::Owner, None, Utc::now())
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn sessions_validate_until_expiry_and_ids_are_not_stored_in_plaintext() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        let user = a_user(&store, "owner@example.test").await;
        store
            .create_session("session-secret", &user, now, now + Duration::days(7))
            .await
            .unwrap();
        let who = store
            .session_user("session-secret", now)
            .await
            .unwrap()
            .expect("a live session knows whose it is");
        assert_eq!(who.email, "owner@example.test");
        assert_eq!(who.role, Role::Owner);
        assert!(
            store
                .session_user("session-secret", now + Duration::days(8))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .session_user("never-issued", now)
                .await
                .unwrap()
                .is_none()
        );
        let stored: String = sqlx::query_scalar("SELECT id_hash FROM sessions")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(stored, token_hash("session-secret"));
        assert_ne!(stored, "session-secret");
    }

    #[tokio::test]
    async fn deleting_sessions_one_all_and_expired() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        let user = a_user(&store, "owner@example.test").await;
        store
            .create_session("s1", &user, now, now + Duration::days(7))
            .await
            .unwrap();
        store
            .create_session("s2", &user, now, now + Duration::days(7))
            .await
            .unwrap();
        store
            .create_session("s3", &user, now, now - Duration::seconds(1))
            .await
            .unwrap();

        store.delete_session("s1").await.unwrap();
        assert!(store.session_user("s1", now).await.unwrap().is_none());
        // Logging out twice must not error.
        store.delete_session("s1").await.unwrap();

        store.delete_expired_sessions(now).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 1, "only the live s2 row should remain");

        store.delete_all_sessions().await.unwrap();
        assert!(store.session_user("s2", now).await.unwrap().is_none());
    }
}

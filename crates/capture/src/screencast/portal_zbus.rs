//! `org.freedesktop.portal.ScreenCast` over D-Bus, using the `zbus` crate the workspace
//! already depends on.
//!
//! Every portal method that needs user interaction follows the *Request* pattern: the call
//! returns a request object path immediately and the result arrives later as a
//! `org.freedesktop.portal.Request.Response(u code, a{sv} results)` signal on that path. The
//! path is predictable from our unique bus name and the `handle_token` we pass, so we
//! subscribe to the signal *before* calling, which avoids missing a fast response.

use std::collections::HashMap;
use std::future::poll_fn;
use std::os::fd::OwnedFd;
use std::pin::Pin;

use async_trait::async_trait;
use tokio::sync::oneshot;
use zbus::export::futures_core::Stream;
use zbus::zvariant::{self, ObjectPath, Value};
use zbus::{Connection, Proxy};

use super::error::ScreenCastError;
use super::portal::{PortalStream, ScreenCastPortal, SelectOptions, SessionHandle, StartResponse};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";

/// The object path a Request for `handle_token` made by bus name `sender` will use.
pub fn request_path(sender: &str, handle_token: &str) -> String {
    let sender = sender.trim_start_matches(':').replace('.', "_");
    format!("{PORTAL_PATH}/request/{sender}/{handle_token}")
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "a{sv}")]
struct SessionResults {
    session_handle: Option<String>,
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "a{sv}")]
struct NoResults {}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "a{sv}")]
struct StreamProps {
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    source_type: Option<u32>,
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "a{sv}")]
struct StartResults {
    streams: Option<Vec<(u32, StreamProps)>>,
    restore_token: Option<String>,
}

fn map_err(e: zbus::Error) -> ScreenCastError {
    let (name, text) = match &e {
        zbus::Error::MethodError(name, desc, _) => {
            (name.to_string(), desc.clone().unwrap_or_default())
        }
        zbus::Error::FDO(fdo) => (format!("{fdo:?}"), fdo.to_string()),
        other => (String::new(), other.to_string()),
    };
    let missing = [
        "ServiceUnknown",
        "NameHasNoOwner",
        "UnknownMethod",
        "UnknownObject",
        "UnknownInterface",
        "NoReply",
        "Disconnected",
    ];
    if missing.iter().any(|m| name.contains(m)) {
        ScreenCastError::PortalUnavailable(format!("{name} {text}").trim().to_owned())
    } else if name.contains("AccessDenied") || name.contains("NotAllowed") {
        ScreenCastError::Denied(format!("{name} {text}").trim().to_owned())
    } else if name.is_empty() && matches!(e, zbus::Error::InputOutput(_) | zbus::Error::Address(_))
    {
        ScreenCastError::PortalUnavailable(text)
    } else {
        ScreenCastError::Portal(format!("{name} {text}").trim().to_owned())
    }
}

type ResponseStream = zbus::proxy::SignalStream<'static>;

/// D-Bus implementation of [`ScreenCastPortal`].
pub struct ZbusPortal {
    conn: Connection,
    counter: u32,
}

impl ZbusPortal {
    /// Connects to the session bus and checks that the ScreenCast portal answers.
    pub async fn connect_session() -> Result<Self, ScreenCastError> {
        let conn = Connection::session().await.map_err(|e| {
            ScreenCastError::PortalUnavailable(format!("cannot connect to the session bus: {e}"))
        })?;
        let mut me = Self::with_connection(conn);
        me.version().await?;
        Ok(me)
    }

    /// Uses an existing connection (tests use a private bus).
    pub fn with_connection(conn: Connection) -> Self {
        Self { conn, counter: 0 }
    }

    async fn screencast(&self) -> Result<Proxy<'static>, ScreenCastError> {
        Proxy::new(&self.conn, DESTINATION, PORTAL_PATH, SCREENCAST_IFACE)
            .await
            .map_err(map_err)
    }

    /// Allocates a handle token and subscribes to the matching Response signal.
    async fn begin_request(&mut self) -> Result<(String, ResponseStream), ScreenCastError> {
        self.counter += 1;
        let token = format!("lq_{}_{}", std::process::id(), self.counter);
        let sender = self
            .conn
            .unique_name()
            .ok_or_else(|| ScreenCastError::Portal("connection has no unique name".into()))?
            .to_string();
        let path = request_path(&sender, &token);
        let proxy = Proxy::new(&self.conn, DESTINATION, path, REQUEST_IFACE)
            .await
            .map_err(map_err)?;
        let signals = proxy.receive_signal("Response").await.map_err(map_err)?;
        Ok((token, signals))
    }
}

async fn next<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// Waits for the Response and maps the portal's response code to a typed result.
async fn response<R>(mut signals: ResponseStream) -> Result<R, ScreenCastError>
where
    R: for<'d> serde::Deserialize<'d> + zvariant::Type,
{
    let msg = next(&mut signals)
        .await
        .ok_or_else(|| ScreenCastError::Portal("portal request ended without a response".into()))?;
    let body = msg.body();
    let (code, results): (u32, R) = body
        .deserialize()
        .map_err(|e| ScreenCastError::Portal(format!("malformed Response: {e}")))?;
    match code {
        0 => Ok(results),
        1 => Err(ScreenCastError::UserCancelled),
        n => Err(ScreenCastError::Denied(format!("portal response code {n}"))),
    }
}

fn session_path(session: &SessionHandle) -> Result<ObjectPath<'_>, ScreenCastError> {
    ObjectPath::try_from(session.0.as_str())
        .map_err(|e| ScreenCastError::Portal(format!("bad session handle {:?}: {e}", session.0)))
}

#[async_trait]
impl ScreenCastPortal for ZbusPortal {
    async fn version(&mut self) -> Result<u32, ScreenCastError> {
        let proxy = self.screencast().await?;
        proxy.get_property::<u32>("version").await.map_err(|e| {
            ScreenCastError::PortalUnavailable(format!(
                "no {SCREENCAST_IFACE} on {DESTINATION} (is xdg-desktop-portal and a compositor \
                 backend running?): {}",
                map_err(e)
            ))
        })
    }

    async fn available_cursor_modes(&mut self) -> Result<u32, ScreenCastError> {
        self.screencast()
            .await?
            .get_property::<u32>("AvailableCursorModes")
            .await
            .map_err(map_err)
    }

    async fn create_session(&mut self) -> Result<SessionHandle, ScreenCastError> {
        let proxy = self.screencast().await?;
        let (token, signals) = self.begin_request().await?;
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        let session_token = format!("lq_session_{token}");
        options.insert("session_handle_token", Value::from(session_token.as_str()));
        proxy
            .call_method("CreateSession", &(options,))
            .await
            .map_err(map_err)?;
        let results: SessionResults = response(signals).await?;
        results.session_handle.map(SessionHandle).ok_or_else(|| {
            ScreenCastError::Portal("CreateSession returned no session_handle".into())
        })
    }

    async fn select_sources(
        &mut self,
        session: &SessionHandle,
        o: &SelectOptions,
    ) -> Result<(), ScreenCastError> {
        let proxy = self.screencast().await?;
        let (token, signals) = self.begin_request().await?;
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("types", Value::from(o.source_types));
        options.insert("multiple", Value::from(o.multiple));
        if let Some(c) = o.cursor_mode {
            options.insert("cursor_mode", Value::from(c));
        }
        if let Some(p) = o.persist_mode {
            options.insert("persist_mode", Value::from(p));
        }
        if let Some(t) = &o.restore_token {
            options.insert("restore_token", Value::from(t.as_str()));
        }
        proxy
            .call_method("SelectSources", &(session_path(session)?, options))
            .await
            .map_err(map_err)?;
        let _: NoResults = response(signals).await?;
        Ok(())
    }

    async fn start(&mut self, session: &SessionHandle) -> Result<StartResponse, ScreenCastError> {
        let proxy = self.screencast().await?;
        let (token, signals) = self.begin_request().await?;
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        proxy
            .call_method("Start", &(session_path(session)?, "", options))
            .await
            .map_err(map_err)?;
        let results: StartResults = response(signals).await?;
        Ok(StartResponse {
            streams: results
                .streams
                .unwrap_or_default()
                .into_iter()
                .map(|(node_id, p)| PortalStream {
                    node_id,
                    size: p.size,
                    position: p.position,
                    source_type: p.source_type,
                })
                .collect(),
            restore_token: results.restore_token.filter(|t| !t.is_empty()),
        })
    }

    async fn open_pipewire_remote(
        &mut self,
        session: &SessionHandle,
    ) -> Result<OwnedFd, ScreenCastError> {
        let proxy = self.screencast().await?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        let fd: zvariant::OwnedFd = proxy
            .call("OpenPipeWireRemote", &(session_path(session)?, options))
            .await
            .map_err(map_err)?;
        Ok(fd.into())
    }

    async fn watch_closed(
        &mut self,
        session: &SessionHandle,
    ) -> Result<oneshot::Receiver<()>, ScreenCastError> {
        let proxy = Proxy::new(&self.conn, DESTINATION, session.0.clone(), SESSION_IFACE)
            .await
            .map_err(map_err)?;
        let mut signals = proxy.receive_signal("Closed").await.map_err(map_err)?;
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            // Keep the proxy alive as long as we listen.
            let _proxy = proxy;
            if next(&mut signals).await.is_some() {
                let _ = tx.send(());
            }
        });
        Ok(rx)
    }

    async fn close_session(&mut self, session: &SessionHandle) {
        let Ok(proxy) = Proxy::new(&self.conn, DESTINATION, session.0.clone(), SESSION_IFACE).await
        else {
            return;
        };
        if let Err(e) = proxy.call_noreply("Close", &()).await {
            tracing::debug!("closing portal session {}: {e}", session.0);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Runs the real client against a fake portal served by zbus on a private `dbus-daemon`.
    //! Skipped (with a message) when `dbus-daemon` cannot be started.

    use super::*;
    use crate::screencast::portal::{negotiate, CURSOR_EMBEDDED, PERSIST_UNTIL_REVOKED};
    use crate::screencast::token_store::RestoreTokenStore;
    use std::io::{BufRead, BufReader};
    use std::os::fd::FromRawFd;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use zbus::message::Header;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue};

    struct Bus {
        child: Child,
        address: String,
    }

    impl Bus {
        fn start() -> Option<Self> {
            let mut child = Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let mut line = String::new();
            BufReader::new(child.stdout.take()?)
                .read_line(&mut line)
                .ok()?;
            let address = line.trim().to_owned();
            if address.is_empty() {
                let _ = child.kill();
                return None;
            }
            Some(Self { child, address })
        }

        async fn connect(&self) -> Connection {
            zbus::connection::Builder::address(self.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap()
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum StartBehavior {
        Grant,
        Cancel,
    }

    #[derive(Default)]
    struct Recorded {
        select: Vec<HashMap<String, String>>,
        calls: Vec<String>,
    }

    struct Script {
        start: StartBehavior,
        /// Reject `SelectSources` carrying exactly this restore token with response code 2.
        reject_token: Option<String>,
        new_token: Option<String>,
        recorded: Recorded,
    }

    struct FakeScreenCast {
        script: Arc<Mutex<Script>>,
    }

    struct FakeSession;

    #[zbus::interface(name = "org.freedesktop.portal.Session")]
    impl FakeSession {
        async fn close(&self) {}
    }

    fn describe(options: &HashMap<String, OwnedValue>) -> HashMap<String, String> {
        options
            .iter()
            .map(|(k, v)| {
                let text = if let Ok(s) = v.downcast_ref::<&str>() {
                    s.to_owned()
                } else if let Ok(n) = v.downcast_ref::<u32>() {
                    n.to_string()
                } else if let Ok(b) = v.downcast_ref::<bool>() {
                    b.to_string()
                } else {
                    format!("{v:?}")
                };
                (k.clone(), text)
            })
            .collect()
    }

    async fn respond(
        conn: &Connection,
        header: &Header<'_>,
        options: &HashMap<String, OwnedValue>,
        code: u32,
        results: HashMap<&'static str, Value<'static>>,
    ) -> OwnedObjectPath {
        let token = options
            .get("handle_token")
            .and_then(|v| v.downcast_ref::<&str>().ok())
            .expect("handle_token")
            .to_owned();
        let sender = header.sender().expect("sender").to_string();
        let path = request_path(&sender, &token);
        let conn = conn.clone();
        let signal_path = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            conn.emit_signal(
                None::<&str>,
                signal_path.as_str(),
                REQUEST_IFACE,
                "Response",
                &(code, results),
            )
            .await
            .unwrap();
        });
        OwnedObjectPath::try_from(path).unwrap()
    }

    #[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
    impl FakeScreenCast {
        // The real portal spells this property in lower case.
        #[zbus(property, name = "version")]
        fn version(&self) -> u32 {
            5
        }

        #[zbus(property)]
        fn available_cursor_modes(&self) -> u32 {
            7
        }

        #[zbus(property)]
        fn available_source_types(&self) -> u32 {
            3
        }

        async fn create_session(
            &self,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] conn: &Connection,
        ) -> OwnedObjectPath {
            self.script
                .lock()
                .unwrap()
                .recorded
                .calls
                .push("CreateSession".into());
            let session = format!(
                "{PORTAL_PATH}/session/test/{}",
                options["session_handle_token"]
                    .downcast_ref::<&str>()
                    .unwrap()
            );
            conn.object_server()
                .at(session.as_str(), FakeSession)
                .await
                .unwrap();
            let mut results = HashMap::new();
            results.insert("session_handle", Value::from(session));
            respond(conn, &header, &options, 0, results).await
        }

        async fn select_sources(
            &self,
            _session: ObjectPath<'_>,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] conn: &Connection,
        ) -> OwnedObjectPath {
            let (code, rejected) = {
                let mut s = self.script.lock().unwrap();
                s.recorded.calls.push("SelectSources".into());
                s.recorded.select.push(describe(&options));
                let token = options
                    .get("restore_token")
                    .and_then(|v| v.downcast_ref::<&str>().ok());
                let rejected = token.is_some() && token == s.reject_token.as_deref();
                (if rejected { 2 } else { 0 }, rejected)
            };
            let _ = rejected;
            respond(conn, &header, &options, code, HashMap::new()).await
        }

        async fn start(
            &self,
            _session: ObjectPath<'_>,
            _parent: String,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] conn: &Connection,
        ) -> OwnedObjectPath {
            let (behavior, token) = {
                let mut s = self.script.lock().unwrap();
                s.recorded.calls.push("Start".into());
                (s.start, s.new_token.clone())
            };
            if behavior == StartBehavior::Cancel {
                return respond(conn, &header, &options, 1, HashMap::new()).await;
            }
            let mut props: HashMap<&str, Value<'_>> = HashMap::new();
            props.insert("size", Value::from((2560i32, 1440i32)));
            props.insert("position", Value::from((0i32, 0i32)));
            props.insert("source_type", Value::from(1u32));
            let streams = vec![(77u32, props)];
            let mut results: HashMap<&'static str, Value<'static>> = HashMap::new();
            results.insert(
                "streams",
                Value::new(streams).try_to_owned().unwrap().into(),
            );
            if let Some(t) = token {
                results.insert("restore_token", Value::from(t));
            }
            respond(conn, &header, &options, 0, results).await
        }

        async fn open_pipe_wire_remote(
            &self,
            _session: ObjectPath<'_>,
            _options: HashMap<String, OwnedValue>,
        ) -> zvariant::OwnedFd {
            self.script
                .lock()
                .unwrap()
                .recorded
                .calls
                .push("OpenPipeWireRemote".into());
            let fd = unsafe { libc::memfd_create(c"fake".as_ptr(), libc::MFD_CLOEXEC) };
            assert!(fd >= 0);
            zvariant::OwnedFd::from(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    async fn fake_portal(bus: &Bus, script: Script) -> (Connection, Arc<Mutex<Script>>) {
        let script = Arc::new(Mutex::new(script));
        let conn = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(DESTINATION)
            .unwrap()
            .serve_at(
                PORTAL_PATH,
                FakeScreenCast {
                    script: Arc::clone(&script),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        (conn, script)
    }

    fn script(start: StartBehavior) -> Script {
        Script {
            start,
            reject_token: None,
            new_token: Some("portal-token-1".into()),
            recorded: Recorded::default(),
        }
    }

    fn token_store(tag: &str) -> (RestoreTokenStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("lq-zbus-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (RestoreTokenStore::at(root.join("tok")), root)
    }

    macro_rules! bus_or_skip {
        () => {
            match Bus::start() {
                Some(b) => b,
                None => {
                    eprintln!("skipping: dbus-daemon is not available");
                    return;
                }
            }
        };
    }

    #[test]
    fn request_path_follows_the_portal_convention() {
        assert_eq!(
            request_path(":1.42", "tok"),
            "/org/freedesktop/portal/desktop/request/1_42/tok"
        );
    }

    #[tokio::test]
    async fn full_flow_against_a_fake_portal_then_reuse_of_the_token() {
        let bus = bus_or_skip!();
        let (_server, script) = fake_portal(&bus, script(StartBehavior::Grant)).await;
        let (store, root) = token_store("flow");

        let mut portal = ZbusPortal::with_connection(bus.connect().await);
        let grant = negotiate(&mut portal, Some(&store)).await.unwrap();
        assert_eq!(grant.stream.node_id, 77);
        assert_eq!(grant.stream.size, Some((2560, 1440)));
        assert!(!grant.used_restore_token);
        assert!(grant.token_persisted);
        assert_eq!(store.load().as_deref(), Some("portal-token-1"));
        use std::os::fd::AsRawFd;
        assert!(grant.pipewire_fd.as_raw_fd() >= 0);

        // The first run asked for persistence, embedded cursor and monitors only.
        {
            let s = script.lock().unwrap();
            let sel = &s.recorded.select[0];
            assert_eq!(sel["types"], "1");
            assert_eq!(sel["multiple"], "false");
            assert_eq!(sel["cursor_mode"], CURSOR_EMBEDDED.to_string());
            assert_eq!(sel["persist_mode"], PERSIST_UNTIL_REVOKED.to_string());
            assert!(!sel.contains_key("restore_token"));
            assert_eq!(
                s.recorded.calls,
                [
                    "CreateSession",
                    "SelectSources",
                    "Start",
                    "OpenPipeWireRemote"
                ]
            );
        }

        // Second run: the stored token is sent back.
        let mut portal2 = ZbusPortal::with_connection(bus.connect().await);
        let grant2 = negotiate(&mut portal2, Some(&store)).await.unwrap();
        assert!(grant2.used_restore_token);
        assert_eq!(
            script.lock().unwrap().recorded.select[1]["restore_token"],
            "portal-token-1"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejected_token_falls_back_to_a_fresh_prompt() {
        let bus = bus_or_skip!();
        let mut sc = script(StartBehavior::Grant);
        sc.reject_token = Some("stale".into());
        let (_server, script) = fake_portal(&bus, sc).await;
        let (store, root) = token_store("stale");
        store.save("stale").unwrap();

        let mut portal = ZbusPortal::with_connection(bus.connect().await);
        let grant = negotiate(&mut portal, Some(&store)).await.unwrap();
        assert!(!grant.used_restore_token);
        let s = script.lock().unwrap();
        assert_eq!(s.recorded.select.len(), 2);
        assert_eq!(s.recorded.select[0]["restore_token"], "stale");
        assert!(!s.recorded.select[1].contains_key("restore_token"));
        drop(s);
        assert_eq!(store.load().as_deref(), Some("portal-token-1"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_picker_is_a_typed_error() {
        let bus = bus_or_skip!();
        let (_server, _script) = fake_portal(&bus, script(StartBehavior::Cancel)).await;
        let mut portal = ZbusPortal::with_connection(bus.connect().await);
        let err = negotiate(&mut portal, None).await.unwrap_err();
        assert_eq!(err, ScreenCastError::UserCancelled);
    }

    #[tokio::test]
    async fn missing_portal_is_unavailable() {
        let bus = bus_or_skip!();
        let mut portal = ZbusPortal::with_connection(bus.connect().await);
        let err = negotiate(&mut portal, None).await.unwrap_err();
        assert!(
            matches!(err, ScreenCastError::PortalUnavailable(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn session_closed_signal_reaches_the_grant() {
        let bus = bus_or_skip!();
        let (server, _script) = fake_portal(&bus, script(StartBehavior::Grant)).await;
        let mut portal = ZbusPortal::with_connection(bus.connect().await);
        let grant = negotiate(&mut portal, None).await.unwrap();
        server
            .emit_signal(
                None::<&str>,
                grant.session.0.as_str(),
                SESSION_IFACE,
                "Closed",
                &(HashMap::<&str, Value<'_>>::new(),),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), grant.closed)
            .await
            .expect("Closed delivered")
            .expect("sender alive");
    }
}

//! BitTorrent 引擎（librqbit 封装，设计文档 3.11，v4.9.27）。
//!
//! 只做三件事：起会话、加种子、控制生命周期。任务队列、aria2 语义、
//! 落盘白名单都在 `core::download`；协议面在 `api::aria2_rpc`。
//!
//! 和 HTTP 引擎的关键差别：BT 的进度不是「我写了多少字节」，而是
//! 「校验通过的 piece 覆盖了多少字节」。所以 BT 任务的进度只从
//! `ManagedTorrent::stats()` 读，不从自己的计数器累加 —— 否则断点续传
//! 时会重复计数（librqbit 启动时会把已有文件重新校验一遍）。
//!
//! 落盘安全：`output_folder` 由 `DownloadManager` 的白名单解析后传入，
//! 这里不再二次校验；多文件种子由 librqbit 自行建子目录（aria2 同款行为）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use librqbit::api::TorrentIdOrHash;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions,
};

/// 下载中的种子句柄（`Arc<ManagedTorrent>`，与 librqbit 的别名等价）
pub type TorrentHandle = Arc<ManagedTorrent>;

/// 种子来源：磁力链（含 40 位 info hash 裸串）或 .torrent 字节
pub enum TorrentSource {
    Magnet(String),
    Bytes(Vec<u8>),
}

/// 加入会话之前就能确定的信息：用来立刻回填 `tellStatus.infoHash` / 种子名。
///
/// 为什么需要这一步：magnet 的 `add_torrent` 要等元信息（可能几十秒），
/// 如果等它返回再填字段，客户端在这段时间看到的是 infoHash=""、名字是占位符，
/// AriaNg 会把它当成「未知任务」。bytes 则顺便当合法性校验——parse 不过
/// 直接 fault，而不是建一个马上变 error 的任务。
pub struct TorrentPreview {
    pub info_hash: String,
    pub name: Option<String>,
}

impl TorrentSource {
    pub fn preview(&self) -> Result<TorrentPreview, String> {
        match self {
            TorrentSource::Bytes(bytes) => {
                let meta = librqbit::torrent_from_bytes::<librqbit::ByteBufOwned>(bytes)
                    .map_err(|err| format!("invalid torrent file: {err}"))?;
                Ok(TorrentPreview {
                    info_hash: meta.info_hash.as_string(),
                    name: meta
                        .info
                        .name
                        .as_ref()
                        .map(|name| String::from_utf8_lossy(name.as_ref()).into_owned()),
                })
            }
            TorrentSource::Magnet(uri) => {
                let magnet = librqbit::Magnet::parse(uri)
                    .map_err(|err| format!("invalid magnet link: {err}"))?;
                let info_hash = magnet
                    .as_id20()
                    .ok_or_else(|| "magnet link has no BTv1 info hash".to_string())?
                    .as_string();
                Ok(TorrentPreview {
                    info_hash,
                    name: magnet.name,
                })
            }
        }
    }
}

/// 一个 BT 会话（整个进程一个）。
pub struct BtEngine {
    session: Arc<Session>,
    /// 实际监听端口（未监听入站时为 None），启动日志用
    listen_port: Option<u16>,
}

impl BtEngine {
    /// 起会话：DHT 状态落在 `<data>/bt/`，不启用 UPnP（不替用户开路由器端口），
    /// 不启用会话持久化（重启后 RPC 侧看不到恢复的任务，会变成幽灵下载）。
    pub async fn start(
        default_root: PathBuf,
        data_dir: &Path,
        ports: Option<std::ops::Range<u16>>,
        disable_dht: bool,
    ) -> Result<Arc<Self>, String> {
        let bt_dir = data_dir.join("bt");
        std::fs::create_dir_all(&bt_dir)
            .map_err(|err| format!("create bt dir {}: {err}", bt_dir.display()))?;

        let mut opts = SessionOptions {
            disable_dht,
            listen_port_range: ports.clone(),
            enable_upnp_port_forwarding: false,
            // 不写 session.json：恢复出来的任务不在我们的 TaskMap 里，
            // 用户会看到「磁盘在涨、RPC 列表里什么都没有」。
            persistence: None,
            ..Default::default()
        };
        if !disable_dht {
            opts.dht_config = Some(librqbit::dht::PersistentDhtConfig {
                dump_interval: None,
                config_filename: Some(bt_dir.join("dht.json")),
            });
            opts.disable_dht_persistence = false;
        }

        let session = Session::new_with_opts(default_root, opts)
            .await
            .map_err(|err| format!("start bittorrent session: {err}"))?;
        let listen_port = session.tcp_listen_port();
        Ok(Arc::new(Self {
            session,
            listen_port,
        }))
    }

    pub fn listen_port(&self) -> Option<u16> {
        self.listen_port
    }

    /// 加一个种子。返回的句柄随任务一起保存，后续 pause/unpause/delete 都用它。
    ///
    /// `trackers` 是实例级的追加列表（aria2 `bt-tracker` 语义）：
    /// - `.torrent` 走 `AddTorrentOptions::trackers`，librqbit 会把它拼进种子
    ///   自带的 announce 列表；
    /// - magnet 只能改 URI——librqbit 8.1.1 的 magnet 分支忽略
    ///   `AddTorrentOptions::trackers`，只读链接里的 `tr=`。
    pub async fn add(
        &self,
        source: TorrentSource,
        dir: &Path,
        only_files: Option<Vec<usize>>,
        paused: bool,
        trackers: &[String],
    ) -> Result<TorrentHandle, String> {
        let add = match source {
            TorrentSource::Magnet(uri) => {
                AddTorrent::from_url(crate::core::download::magnet_with_trackers(&uri, trackers))
            }
            TorrentSource::Bytes(bytes) => AddTorrent::from_bytes(bytes),
        };
        let options = AddTorrentOptions {
            output_folder: Some(dir.to_string_lossy().into_owned()),
            // aria2 的语义是「继续写已有文件」（断点续传），不是「拒绝覆盖」
            overwrite: true,
            paused,
            only_files,
            // 空列表给 None：别把「没配 tracker」变成「显式空列表」
            trackers: (!trackers.is_empty()).then(|| trackers.to_vec()),
            ..Default::default()
        };
        match self
            .session
            .add_torrent(add, Some(options))
            .await
            .map_err(|err| err.to_string())?
        {
            AddTorrentResponse::ListOnly(_) => Err("torrent was added in list-only mode".into()),
            other => other
                .into_handle()
                .ok_or_else(|| "torrent was not added to the session".to_string()),
        }
    }

    pub async fn pause(&self, handle: &TorrentHandle) -> Result<(), String> {
        self.session
            .pause(handle)
            .await
            .map_err(|err| err.to_string())
    }

    pub async fn unpause(&self, handle: &TorrentHandle) -> Result<(), String> {
        self.session
            .unpause(handle)
            .await
            .map_err(|err| err.to_string())
    }

    /// 移除任务，**不删已下载文件**（对齐 aria2 的 remove / forceRemove）。
    pub async fn remove(&self, handle: &TorrentHandle) {
        let _ = self
            .session
            .delete(TorrentIdOrHash::Hash(handle.info_hash()), false)
            .await;
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librqbit::{AddTorrentOptions, CreateTorrentOptions, create_torrent};
    use std::time::Duration;

    /// 造一个「做种端 + 下载端」都在本机回环的种子。
    ///
    /// 不走 DHT / tracker：两边用 `initial_peers` 直连，测试不依赖外网。
    async fn local_pair(
        tag: &str,
    ) -> (
        PathBuf,
        Vec<u8>,
        Arc<Session>,
        TorrentHandle,
        Arc<Session>,
        TorrentHandle,
    ) {
        let base = std::env::temp_dir().join(format!("{tag}-{:016x}", rand::random::<u64>()));
        let seed_dir = base.join("seed");
        let out_dir = base.join("out");
        std::fs::create_dir_all(&seed_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();

        let payload: Vec<u8> = (0..192 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(seed_dir.join("payload.bin"), &payload).unwrap();
        let torrent = create_torrent(
            &seed_dir.join("payload.bin"),
            CreateTorrentOptions {
                name: None,
                piece_length: Some(32 * 1024),
            },
        )
        .await
        .unwrap();
        let bytes = torrent.as_bytes().unwrap();

        // 端口段随机起，避免并行测试互抢
        let port_base = 20_000 + (u16::from_be_bytes([rand::random(), rand::random()]) % 20_000);
        let seeder = Session::new_with_opts(
            seed_dir.clone(),
            SessionOptions {
                disable_dht: true,
                disable_dht_persistence: true,
                listen_port_range: Some(port_base..port_base + 10),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let seed_port = seeder.tcp_listen_port().expect("seeder listens");
        let seeding = seeder
            .add_torrent(
                AddTorrent::from_bytes(bytes.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(seed_dir.to_string_lossy().into_owned()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        // 等做种端把已有文件校验完（否则下载端拿不到 piece）
        for _ in 0..100 {
            if seeding.stats().finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let downloader = Session::new_with_opts(
            out_dir.clone(),
            SessionOptions {
                disable_dht: true,
                disable_dht_persistence: true,
                listen_port_range: Some(port_base + 10..port_base + 20),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let downloading = downloader
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    output_folder: Some(out_dir.to_string_lossy().into_owned()),
                    overwrite: true,
                    initial_peers: Some(vec![format!("127.0.0.1:{seed_port}").parse().unwrap()]),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        (base, payload, seeder, seeding, downloader, downloading)
    }

    /// 实例级 Tracker 必须真的落到种子上（v4.9.39）。
    ///
    /// 只验「加进会话的种子带上了这些 announce」——真正的 announce 需要外网，
    /// 这里不测网络；`paused` 让种子停在元信息阶段，不会尝试连 peer。
    #[tokio::test]
    async fn instance_trackers_are_attached_to_added_torrents() {
        let base = std::env::temp_dir().join(format!("bt-trackers-{:016x}", rand::random::<u64>()));
        let seed_dir = base.join("seed");
        let out_dir = base.join("out");
        std::fs::create_dir_all(&seed_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(seed_dir.join("payload.bin"), vec![7u8; 64 * 1024]).unwrap();
        let torrent = create_torrent(
            &seed_dir.join("payload.bin"),
            CreateTorrentOptions {
                name: None,
                piece_length: Some(32 * 1024),
            },
        )
        .await
        .unwrap();
        let bytes = torrent.as_bytes().unwrap();

        let engine = BtEngine::start(out_dir.clone(), &base, None, true)
            .await
            .unwrap();
        let handle = engine
            .add(
                TorrentSource::Bytes(bytes.to_vec()),
                &out_dir,
                None,
                true,
                &["udp://tracker.example:6969/announce".to_string()],
            )
            .await
            .unwrap();

        // 种子自带 announce 为空，所以这里应该正好只有我们追加的那条
        let announce: Vec<String> = handle
            .shared()
            .trackers
            .iter()
            .map(|url| url.as_str().to_string())
            .collect();
        assert_eq!(announce, vec!["udp://tracker.example:6969/announce"]);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn local_seeder_and_downloader_transfer_bytes() {
        let (base, payload, seeder, _seeding, downloader, downloading) =
            local_pair("asmoday-bt-pair").await;

        tokio::time::timeout(Duration::from_secs(90), downloading.wait_until_completed())
            .await
            .expect("torrent finished in time")
            .expect("torrent finished without error");

        let got = std::fs::read(base.join("out").join("payload.bin")).unwrap();
        assert_eq!(got.len(), payload.len());
        assert_eq!(got, payload, "downloaded bytes must match the seed");

        seeder.stop().await;
        downloader.stop().await;
        let _ = std::fs::remove_dir_all(&base);
    }
}

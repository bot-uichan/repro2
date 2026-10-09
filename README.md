# repro2: Tailscale IdP による最小 report / threshold スライス

このブランチは `feat/regsitry-consensus` を土台にした最小実装です。registry は報告の保存・提供だけを担当し、採用ポリシーは gateway が担当します。TEE、複数 gateway、consensus daemon、UI はありません。

## 認証と信頼境界

registry の `POST /build-reports` は、信頼された Tailscale Serve が付与する `Tailscale-User-Login` をユーザー識別子として保存します。ヘッダーがない、空、空白のみ、テキストでない場合は **401**。JSON に `user_id` を指定しても認証には使いません。未認証で受理する本番用 fallback はありません。

**registry backend は `127.0.0.1:3001` にのみ bind します。この制約を外さないでください。** 任意のクライアントが直接 backend に接続できると、ヘッダーを偽装できます。同じホストのプロセス・運用者も信頼境界内です。代わりの local reverse proxy を使う場合も、外部から渡された identity ヘッダーを破棄し、認証済みの値だけを設定する必要があります。

[Tailscale Serve の公式 identity headers 説明](https://tailscale.com/docs/features/tailscale-serve#identity-headers) に従い、ユーザー所有の端末から **Serve の URL** に投稿してください。tagged devices はこのヘッダー方式ではユーザー投票者として未対応です。Funnel は使いません。ユーザーが複数端末を持っていても、同じ login は同じユーザーとして扱います。Serve / tailnet のアクセス許可と gateway 運用者は信頼する前提です。

## 報告と gateway ポリシー（phase 2）

報告フィールド:

- `drv_path`: nullable string
- `output_name`, `store_path_hash`, `store_path`, `nar_hash`: string
- `nar_size`: signed integer（gateway が負数・不正な Nix metadata を除外）
- `cache_url`: optional HTTP(S) cache base URL。認証情報・query・fragment は不可
- `metadata`: optional `{ "references": ["/nix/store/..."], "deriver": null | "/nix/store/...drv" }`。references は完全な store path の集合として sort / dedup します
- `artifact`: optional `{ "file_hash": "64 lowercase hex", "file_size": positive integer, "compression": "none" }`。metadata が必須で、file-server 上の download representation を表します

registry は `(user_id, drv_path, output_name, store_path_hash, store_path, nar_hash, nar_size, canonical metadata)` に SQLite unique index を持ち、同一ユーザー・同一結果の再投稿を upsert します。nullable 入力の NULL と空文字は区別します。cache URL / artifact は投票のキーではなく、再投稿時に最後の値（NULL を含む）で置き換えます。metadata ありの NAR hash は SRI に canonicalize します。別の結果は別の報告として保存されます。既存行は migration 後も `user_id = NULL` のまま保持し、架空の所有者を補いません。

`GET /nar-info/{store_path_hash}` は identity・入力・結果・nullable cache URL を含む **全報告**を返します。未公開の報告も投票の材料として返すため、以前の cache URL 非 NULL のフィルタはありません。この endpoint は registry 自身が合意を判定するものではありません。

gateway は環境変数 **`REQUIRED_USERS=N`** を必須とします。未設定・0・負数・非整数・範囲外は listen 前に起動エラーです。

- 同じ drv/output と store path/hash・NAR hash/size・metadata（references / deriver）に対し、**N 人の異なる認証済みユーザー**が一致すると採用可能です。端末数・レコード数では数えません。
- 同じユーザーが同じ結果を何回報告しても、その結果への投票は 1 票です。他の結果にも報告していても、その結果で 2 票にはなりません。
- 所有者なし・空 identity・不正 metadata は票にしません。store path と hash の整合性、要求された hash、上流 narinfo の path/hash/size も確認します。
- 未公開のユーザー報告も一致票になります。ただし採用する結果には、少なくとも 1 人の認証済みユーザーの cache URL、または `BLOB_BASE_URL` と有効な artifact が必要です。legacy の metadata なし報告は metadata あり候補と票を合算しません。
- しきい値未達・公開 cache 不在は **404**。別ユーザーの不一致は停止条件ではなく、同票数による **409** ポリシーもありません。

**実装上の選択詳細（ユーザーが承認した新しいポリシーではありません）:** 複数結果がしきい値に達した場合は `(drv_path, output_name, store_path_hash, store path basename, canonical SRI NAR hash, numeric NAR size, canonical metadata, blob publication preference, cache_url)` の昇順で最初の公開・所有者付き報告を選びます。nullable 入力は NULL が先です。同一結果内では `BLOB_BASE_URL` 設定時に artifact 付き報告を優先します。票数最大の結果を選ぶ仕組みではありません。選んだ cache の不在・不整合時に他の候補へ自動 failover はしません。

gateway は **narinfo だけ**を提供し、NAR URL は file-server または上流 cache の直接 URL にします。legacy cache URL 経路は上流の署名などを維持し、metadata がある場合は references / deriver も一致を検査します。blob 経路では署名・CA 宣言を生成しません。独立署名や NAR proxy は追加しません。Nix クライアント側の署名検証設定は別途必要です。投稿可能ユーザーが選べる cache URL への outbound 接続は可能なので、信頼する tailnet 参加者・cache とネットワーク上の egress 制御を前提とします。このスライスは SSRF 対策用の cache allowlist を実装していません。

## 起動例

Rust tooling を使用します（Nix 環境なら `nix develop` 内で実行）。各長時間プロセスは別 terminal で実行してください。

```sh
export DATABASE_URL='sqlite://reports.sqlite?mode=rwc'
cargo run -p migration -- up
cargo run -p registry
```

registry ホストで、tailnet に対して local backend を公開します。このコマンドは運用者が実行する例であり、実装テストが Serve 設定を変更することはありません。

```sh
tailscale serve 3001
```

```sh
REQUIRED_USERS=2 REGISTRY_URL='https://registry-host.example-tailnet.ts.net' cargo run -p gateway
```

builder は identity ヘッダーを自分で生成しません。Serve の endpoint へ投稿し、必要なら **既に出力を提供している** cache URL を指定します。

```sh
cargo run -p builder -- 'nixpkgs#hello' \
  --registry-url 'https://registry-host.example-tailnet.ts.net' \
  --cache-url 'https://existing-cache.example/nix/'
```

`--cache-url` を省略すると、cache location なしの報告になります。builder はビルド結果を static cache へコピー・公開しません。既存 builder のビルド設定（substitution を許可）は変更していないため、このスライスだけでは独立再ビルドの証明になりません。

## CA NAR blob file-server（phase 1）

`file-server` はローカル filesystem 上の **content-addressed blob 配信だけ**を担当します。phase 2 では registry に IA store path → 複数 NAR candidate metadata を保存し、gateway が直接 blob URL の narinfo を生成します。hook、GC-protected queue、resident retry sender は **後続 phase・未実装**です。

### HTTP protocol

- **`PUT /nar/{sha256-lowercase-hex}.nar`**: uncompressed NAR bytes を送ります。key は受信した **bytes 全体の SHA256**（64 文字 lowercase hex）です。Nix base32 / SRI / store path hash ではありません。blob 経路の narinfo は `Compression: none`、同じ bytes の `FileHash` / `NarHash` と size を使います。
- 初回 upload は **201**。同じ bytes の再送は **200**（inode / mtime を変更しません）。同時送信でも完成した file だけを no-clobber publish し、上書きしません。
- malformed key / encoded path separator / traversal は **400**。hash mismatch は **422**（既存 blob にも触れません）。既存 entry の内容が異なる、symlink / directory 等の場合は **409**。storage I/O failure は **500**。
- upload には trusted proxy が設定した単一の非空 `Tailscale-User-Login` が必要です。未設定・空白・non-text・重複 header は **401**。
- **`GET /nar/{sha256-lowercase-hex}.nar`**: bytes を直接 streaming download します（`Content-Type: application/x-nix-nar`、`Content-Length`）。**HEAD** も対応します。存在しない valid key は **404**。download 自体は user header を要求しません。read access は Serve / tailnet ACL で制限してください。
- upload / download は streaming です。known `Content-Length` と実受信 bytes の両方で upload limit を検査し、chunked upload も limit 超過は **413**。upload 中の file は final URL に露出しません。通常の失敗・cancel では temporary file を削除します。

この phase は **NAR syntax / store-path semantics を parse しません**。opaque bytes を hash-validate して保存する transport です。送信者は実 NAR を送る必要があり、NAR metadata の構造・hash/size の整合性と採用判断は phase 2 の registry / gateway が担当します。ただし bytes から references / deriver / store-path semantics を導出・検証するものではありません。配信時の再 hash は行わないため、運用者による disk 改変・bit rot はこの phase の保証外です。

### Configuration and deployment boundary

```sh
FILE_SERVER_ROOT='/path/to/private/blobs' \
FILE_SERVER_BIND='127.0.0.1:3002' \
FILE_SERVER_MAX_UPLOAD_BYTES='536870912' \
cargo run -p file-server
```

`FILE_SERVER_ROOT` は必須・非空。未作成 directory は mode **0700** で作成し、既存 directory の permissions は変更しません。bind の default は `127.0.0.1:3002`、upload limit の default は **536870912 bytes**。limit は正の `u64`。不正 config は listen 前に失敗します。bind は IPv4 / IPv6 **loopback のみ**を許可します。

**Linux / Unix の信頼する local filesystem と専用 service account を前提**とします。root と親 directories を service account 所有・他ユーザー書き込み不可にし、既存 root も mode 0700 にしてください。symlink / nonregular blob は拒否しますが、host 管理者・同一 user の悪意ある disk mutation への sandbox ではありません。ファイルの no-clobber publication を提供できる local filesystem を使用し、NFS / untrusted network filesystem に配置しないでください。

本番では **Tailscale Serve の HTTP reverse proxy 経由だけ**で公開し、tailnet grants / ACL で upload / read を許可するユーザーを限定してください。identity header の値自体は署名認証ではありません。loopback でも同一 host の任意 process は偽装できるため、local users / host operator は信頼境界内です。別 proxy を使うなら外部 identity header を必ず破棄し、認証済み identity だけを付け直してください。Funnel、公開 listener、外部から backend に直接接続できる forwarding は禁止です。tagged devices は user identity upload として未対応です。

Serve の運用例（実装・test は実行しません）:

```sh
tailscale serve 3002
```

Serve の仕様は既存の [official identity-header documentation](https://tailscale.com/docs/features/tailscale-serve#identity-headers) を参照してください。この phase は per-user authorization / quota、global storage quota、concurrency cap、request timeout、blob GC は実装しません。trusted uploader と deployment の disk quota / rate limit / timeout で resource exhaustion を抑制してください。upload 完了後の process kill / restart の persistence は検証していますが、power loss の durability・crash 時の temp-file scavenging は保証しません。crash 後の dot-prefixed temp files の削除は server を止めて運用者が行ってください。

`cargo test -p file-server` は real local HTTP listener と生成した valid uncompressed regular-file NAR bytes を使用します。process tests は実バイナリを起動し、config / private-root 作成 / upload limit / restart 後の GET・HEAD・dedup を検証します。実 Nix command や実 tailnet / Serve deployment の検証ではありません。

## IA path → NAR candidate と直接 blob narinfo（phase 2）

`metadata` / `artifact` は nullable JSON text columns に保存します。migration は既存 row の identity / metadata を捏造せず保持し、metadata も unique result key に加えます。候補を失う downgrade は拒否するため、この migration の `down` は未対応です。必要なら migration 前の backup から復元してください。

phase 1 の upload が完了したら、認証済みユーザーとして registry に report を POST します。既存 builder は新 metadata を送らないため、phase 2 の自動送信 hook / worker はまだありません。report 例（hash / size / paths は実際の NAR と出力の値に置換）:

```json
{
  "drv_path": "/nix/store/<drv-hash>-example.drv",
  "output_name": "out",
  "store_path_hash": "<32-character Nix store hash>",
  "store_path": "/nix/store/<store-hash>-example",
  "nar_hash": "sha256-<base64 SHA256 of uncompressed NAR>",
  "nar_size": 1234,
  "metadata": {"references": [], "deriver": null},
  "artifact": {"file_hash": "<64 lowercase hex SHA256 of downloaded bytes>", "file_size": 1234, "compression": "none"}
}
```

`NarHash` / `NarSize` は uncompressed NAR の identity、`FileHash` / `FileSize` / compression は download representation です。今は SHA256・`none` のみを受理するため両 hash / size は一致しますが、field は分離します。hex blob key を Nix base32 store hash と混同しません。全 store path / references は `/nix/store/` の完全 path、deriver は `.drv` path を要求します。不正 metadata / artifact は POST **400**、stored JSON の破損は GET **500** です。

```sh
REQUIRED_USERS=2 REGISTRY_URL='https://registry-host.example-tailnet.ts.net' \
BLOB_BASE_URL='https://file-server-host.example-tailnet.ts.net/' cargo run -p gateway
```

`BLOB_BASE_URL` は任意で、未設定なら legacy cache URL 経路だけを使用します。設定すると metadata / artifact 付き採用候補から `<base>/nar/<file_hash>.nar` を生成します。HTTP(S)、host、credentials / query / fragment なし、whitespace / control / backslash / percent encoding / dot path components なしを要求し、末尾 slash を補います。base に subpath を指定できます。gateway と Nix client の双方から base へ接続できる必要があります。

gateway は file-server へ **HEAD** を送り、存在と `Content-Length == FileSize` を確認してから narinfo を返します。不在 / size 不一致は **404**、network / non-success upstream は **502**。download bytes の再 hash や NAR parsing はしません。file-server の immutable hash-validated upload と信頼する storage / operator を前提にします。選択した blob 不在時の別候補 / legacy への自動 failover はありません。

生成する narinfo は `Compression: none` と明示的 `FileHash` / `FileSize` / `NarHash` / `NarSize` を持ち、references / deriver は Nix narinfo の basename 形式です。**IA path を CA path と宣言する `CA:` は追加しません。** `Sig:` も生成しません。この gateway の応答だけでは通常の Nix signature trust を満たさず、client 側 trust / signature 運用は別途必要です。TEE、新 proxy、NAR bytes の gateway 転送はありません。

## 検証

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
python3 tests/http_slice.py
python3 tests/blob_slice.py
```

`tests/http_slice.py` は実際の migration / registry / gateway バイナリ、SQLite、一時的な local HTTP upstream を使います。3000 / 3001 が使用中なら実行しません。identity 必須、URL 検証、legacy 除外、再投稿 dedup、同一ユーザーの別結果、N=2、未公開ユーザーの一致票、不一致・同票、上流 NAR URL を確認します。ヘッダーは local trusted proxy を模して test が注入します。

`tests/blob_slice.py` は実 migration / SQLite / registry / gateway / file-server を起動し、N=2、未公開票、同一 IA path の複数 NAR 候補、missing artifact / blob の 404、直接 GET / HEAD、download SHA256、references / deriver の basename serialization、gateway restart の安定性を検証します。

**実 tailnet / Serve による IdP 検証、実 Nix ビルド・import・署名検証は未実施です。blob test は wire encoding で生成した regular-file NAR fixture を実 HTTP 配信しますが、実 Nix command の成果ではありません。legacy 上流 narinfo は明示的な mock であり、実 Nix の成功を装うものではありません。**

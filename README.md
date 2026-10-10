# repro2: Tailscale IdP report / threshold と自動 NAR publisher

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

`file-server` はローカル filesystem 上の **content-addressed blob 配信だけ**を担当します。phase 2 では registry に IA store path → 複数 NAR candidate metadata を保存し、gateway が直接 blob URL の narinfo を生成します。phase 3 の hook / protected queue / resident sender は下記の別バイナリ `repro2-sender` で実装しています。

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

Serve の仕様は既存の [official identity-header documentation](https://tailscale.com/docs/features/tailscale-serve#identity-headers) を参照してください。この phase は per-user authorization / quota、global storage quota、concurrency cap、request timeout、blob GC は実装しません。trusted uploader と deployment の disk quota / rate limit / timeout で resource exhaustion を抑制してください。phase 3 では upload bytes の fsync に加え、publish 先 directory の fsync 完了後だけ 201 / 200 を返します。directory fsync failure は 500 となり、既に publish された同一 blob の再送でも fsync を再試行します。local filesystem / hardware が fsync を正しく実装する前提です。実 power-cut 耐久試験や crash 時の temp-file scavenging は未実施です。crash 後の dot-prefixed temp files の削除は server を止めて運用者が行ってください。

`cargo test -p file-server` は real local HTTP listener と生成した valid uncompressed regular-file NAR bytes を使用します。process tests は実バイナリを起動し、config / private-root 作成 / upload limit / restart 後の GET・HEAD・dedup を検証します。実 Nix command や実 tailnet / Serve deployment の検証ではありません。

## IA path → NAR candidate と直接 blob narinfo（phase 2）

`metadata` / `artifact` は nullable JSON text columns に保存します。migration は既存 row の identity / metadata を捏造せず保持し、metadata も unique result key に加えます。候補を失う downgrade は拒否するため、この migration の `down` は未対応です。必要なら migration 前の backup から復元してください。

phase 1 の upload が完了したら、認証済みユーザーとして registry に report を POST します。phase 3 の worker はこの手順を自動化します。既存 `builder` の legacy report / cache URL 動作は変更していません。report 例（hash / size / paths は実際の NAR と出力の値に置換）:

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

## 自動 post-build hook / protected queue / resident sender（phase 3）

通常運用は **一度だけ hook と常駐 service を設定**し、その後は通常の Nix build を実行するだけです。各 build ごとの手動 enqueue / send / upload や追加の確認は不要です。`builder` の既存 installable / `--cache-url` 動作と `cargo run -p builder` の default binary は維持しています。

### 自動経路と acceptance requirements

1. Nix が build 後に渡す **`DRV_PATH` と `OUT_PATHS`** を `repro2-sender hook` が検証し、private spool の unique job directory に記録します。job JSON は mode 0600、directory は 0700。temporary file → fsync → atomic rename → directory fsync で公開します。全 outputs と `.drv` に **direct GC-root symlink** を作成して directory を fsync してから hook を戻します。hook は **Nix subprocess も network request も呼びません**。shell glob expansion も使いません。
2. hook と worker の per-job nonblocking lock により、記録 / rooting 中の job を worker が削除する race を防ぎます。worker 自身の exclusive lock は enqueue と別なので、network 障害が hook を待たせません。rooting に失敗しても既に保存された manifest は消しません。worker が次の起動 / scan で再試行します。
3. worker は起動時と約 1 秒ごとの scan で job を自動発見します。**同じ Nix store** の `nix derivation show` で output names を実 path と照合し、`nix path-info --json --recursive` と `nix store dump-path` から実 NAR / hash / size / references / deriver を取得します。dump bytes の SHA256 / size が path-info と一致しなければ公開せず retry します。全 reader command は `--option post-build-hook ''` を指定し、build / copy を呼びません。
4. closure 全体の uncompressed NAR を `<blob endpoint>/nar/<lowercase SHA256 hex>.nar` へ PUT し、HEAD で同じ key / size を readback します。NAR は spool 内 file に dump し streaming hash / upload するため、NAR 全体を RAM に保持しません。その後 **この hook の built outputs のみ** registry へ authenticated metadata / artifact を POST し、`GET /nar-info/<store hash>` で実際の候補を readback します。先に upload するため、retry で既存 artifact を NULL に戻しません。
5. worker は **identity を自己申告しません**。HTTPS の信頼する Tailscale Serve endpoints に接続し、Serve が付与する network identity を registry / file-server が使います。identity header / user / token を注入する本番 option はありません。redirect と environment HTTP proxy は無効です。backend loopback URL を指定するだけでは正常に認証できず 401 retry になります。
6. network / Nix / HTTP / readback failure は **job と全 roots を保持**します。`retry.json` に attempts / Unix-seconds next_attempt / last_error を atomic 保存し、network delivery は exponential **2–256 秒**の backoff で自動 retry、restart 後も期限を継続します。同じ内容の再送は既存 immutable blob / registry upsert により新しい票を作りません。SIGTERM / SIGINT は正常終了を要求し、hung Nix reader を停止します（reader timeout 120 秒、HTTP timeout 30 秒）。SIGKILL / service restart でも committed job は残ります。
7. 全 blob upload と output report の acknowledgement / readback 後に **durable `done.json`** を保存してから roots / queue を解放します。cleanup 中の restart は done marker から再開します。unexpected / mismatched root、symlink directory、untrusted writable ancestors、nonregular / symlink state file、traversal ID は拒否します。corrupt manifest は削除せず CRITICAL log を出し、他の valid jobs を処理します。

[official Nix post-build-hook semantics](https://nix.dev/manual/nix/2.24/advanced-topics/post-build-hook) では hook が build loop を block し、nonzero exit が loop を終了することを説明しています。[official GC-root semantics](https://nix.dev/manual/nix/2.24/package-management/garbage-collector-roots) に従い、`gcroots` 内の subdirectories に direct symlinks を置きます。hook 内で `nix-store --realise` 等を呼び出して daemon / recursive hook を待つ方法は使いません。

### NixOS: flake input で宣言的に有効化（推奨）

GitHub の flake input を追加し、module を import して enable / 2 つの Serve endpoints を設定します。既存の `configuration.nix` / hardware configuration は保持してください。

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    repro2.url = "github:bot-uichan/repro2/feat/tailscale-idp";
  };

  outputs = { nixpkgs, repro2, ... }: {
    nixosConfigurations.my-builder = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./configuration.nix
        repro2.nixosModules.default
        {
          services.repro2-sender = {
            enable = true;
            # 自分の trusted Serve endpoints に置換する placeholders。
            registryUrl = "https://registry-host.example-tailnet.ts.net/";
            blobUrl = "https://blob-host.example-tailnet.ts.net/";
          };
        }
      ];
    };
  };
}
```

自分の trusted Serve endpoints に置き換え、通常の `nixos-rebuild switch --flake .#my-builder` で適用します。その後は普通の Nix build だけで hook → 永続 queue → 常駐 publisher が動作します。別名 `repro2.nixosModules.repro2-sender` も同じ module です。`nix build github:bot-uichan/repro2/feat/tailscale-idp#repro2-sender` で sender 単体を取得できます。package は Linux (`x86_64-linux` / `aarch64-linux`) 用で、workspace の `Cargo.lock` とこの flake の pinned nixpkgs Rust compiler を使用します。`devShells` と既存 `cargo run -p builder` の動作は保持します。consumer の古い Rust toolchain に不用意に `repro2.inputs.nixpkgs.follows` を設定しないでください。Rust 2024 / std の file-lock API に加え、この lock の dependencies の MSRV が適用されます。この locked nixpkgs では Rust **1.97.1** で package build / tests を検証しています。

- `enable` の default は **false**。disabled 時は hook / unit / directories を追加しません。`registryUrl` / `blobUrl` の default は **null** で、enabled 時は両方必須です。HTTP(S) + host、任意 port と単純な未エンコード path のみ。credentials / whitespace / query / fragment / percent encoding / dot components は evaluation で拒否します。URL は public Nix store / unit に保存されるので、secrets を入れないでください。認証情報や自己申告 identity を設定する option はありません。
- `package` は sender package を置換する任意 option。`spoolDirectory` は `/var/lib/repro2-sender`、`gcRootsDirectory` は `/nix/var/nix/gcroots/repro2` が defaults です。変更先はそれぞれ `/var/lib/` / `/nix/var/nix/gcroots/` 配下の normalized path に限定します。tmpfiles が root-owned **0700** の永続 directories を用意し、daemon / sender の起動前に実行します。既存の symlink / writable ancestors 等は Rust queue の runtime validation でも拒否します。NFS / untrusted filesystem は非対応です。
- hook と sender は **同じ root UID と標準 system store** 用です。hook は `/nix/store/...` の fail-safe wrapper と sender binary、unit は絶対 sender / `config.nix.package` binaries + `--store daemon` を使用します。alternative `NIX_STATE_DIR` / isolated or remote stores はこの module の対象外です。disabled daemon / non-root daemon は evaluation で拒否します。service は root で常駐し、private umask、read-only system filesystem + spool / roots のみ writable、restart-on-failure を設定します。
- **既存 `nix.settings.post-build-hook` との併用は assertion / merge error で拒否**します。既存 hook を黙って上書きしたり暗黙に連結したりしません。既存 hook を整理するか、この module を disable し、明示的に設計した wrapper / service を運用してください。手書き `nix.extraOptions` の `post-build-hook` も事前に除去し、typed `nix.settings` で一元管理してください。
- module は **sender のみ**。Tailscale の credentials / enrollment / ACL / Serve、server / gateway、consumer の `substituters` / `trusted-public-keys` / `require-sigs` は変更しません。builder device は user-owned tailnet device を別途用意し、Serve endpoints に接続可能にしてください。network が未準備なら queue / roots を保持して自動 retry します。443 の gateway URL は publication endpoints の代替ではありません。
- `journalctl -u repro2-sender.service` と daemon / build logs の **CRITICAL local queue failure** を監視してください。network 障害は build を失敗させませんが、local recording failure では publication / GC retention を保証できません。disable / directory 変更前に pending jobs を送信完了してください。宣言の削除時の GC-root 解放と queue 保持は下記の lifecycle を参照してください。

### NixOS declaration removal: GC-root lifecycle

この版を **enabled のまま一度 `nixos-rebuild switch` で適用してから**、`services.repro2-sender` の宣言を削除する、`enable = false` にする、または module import 自体を削除して `switch` / `test` すると、旧 enabled generation の `repro2-gc-guardian-<root-path hash>.service` が専用 GC roots を解放します。**`/var/lib/repro2-sender`（custom spool も含む）の manifests / retry state / queue は削除しません。GC 後は未送信 jobs の drv / outputs を失い、queue を残しても復旧・再送不能になる場合があります。** 解放時はこの警告を journal に出します。以前の版から直接 import を削除しても、存在しなかった guardian を後から実行することはできません。

- guardian は root path ごとに作成し、enabled → enabled の package upgrade では `restartIfChanged = false` で保持します。NixOS の **`X-StopOnRemoval=false`** により import 削除後も動作を続け、`/run/current-system` にある enabled marker の消失を監視します。新 disabled module の activation hook に依存せず、**`ExecStop` / `ExecStopPost` では一切削除しません**。普通の sender / daemon stop・restart と、enabled のままの reboot / shutdown では roots を保持します。guardian 自身の SIGTERM も削除を実行しません。
- standard `switch-to-configuration` の `/run/nixos/switch-to-configuration.lock` を取れた後、activated generation の marker がなく、systemd が running / degraded で、sender が inactive / failed になっている場合だけ解放します。systemd が stopping / starting 等の場合は保持します。post-build wrapper は shared lifecycle lock と同じ marker を確認し、disabled 後の **旧 Nix daemon worker が持つ旧 hook も enqueue しません**。実 sender child にも lock FD を渡すため wrapper が先に kill されても in-flight hook を待ちます。ここで共有するのは短い local hook と teardown だけで、network delivery を待つ lock ではありません。
- configured root path のみを、root-owned・non-writable ancestors / private directories を確認した `O_NOFOLLOW` directory FDs に固定して処理します。expected `pid-nanoseconds / numbered store symlinks` tree を全体検査してから unlink / rmdir します。recursive deletion、store path の dereference、別 GC-root subtree の削除はしません。symlink directory、traversal、unexpected entries / targets、unsafe permissions は拒否して保持・警告します。`auto` / `per-user` / `profiles` は予約 subtree として evaluation でも拒否します。custom root path は **repro2 専用の非重複 directory** にしてください。root path を変更した enabled generations の guardian も、enabled marker がある間は旧 roots を保持します。
- **運用上の限界:** `boot` は live switch ではなく、reboot / shutdown を削除契機にしません。`nixos-rebuild boot` だけで disabled generation を予約して reboot した場合は旧 roots が残り、手動の安全な整理が必要です。guardian を管理者が止めたまま宣言を削除した場合、shutdown が live removal の完了前に始まった場合、filesystem / lock / systemd query が失敗した場合も自動解放を保証せず、retention を優先します。module が管理する wrapper / worker の経路だけが対象で、手動で起動した独自 sender、custom activation tools、malicious local root は保証外です。

設計根拠: locked nixpkgs の [switch-to-configuration-ng source](https://github.com/NixOS/nixpkgs/blob/d6524aaca2ff07876657ae2b323f24be4874944b/pkgs/by-name/sw/switch-to-configuration-ng/src/main.rs) は旧 unit の `X-StopOnRemoval` を見て stop を選択し、switch lock を stop → activation → reload / restart / start の全体で保持します。[NixOS switch sequence](https://github.com/NixOS/nixpkgs/blob/d6524aaca2ff07876657ae2b323f24be4874944b/nixos/doc/manual/development/what-happens-during-a-system-switch.chapter.md) と [systemd.service](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html) にある通常 stop / restart / shutdown 時の stop commands を区別しています。

`nix build .#checks.x86_64-linux.nixos-lifecycle` は real NixOS evaluation から disabled / import-removed units の absence、enabled guardian unit と generated Python を取得・syntax-check する **fixture derivation** です。runtime tests は別途、real root 権限を持つ **scratch chroot 内**で `python3 tests/lifecycle.py <fixture-output>/lifecycle.py` と同じ command の `custom-lifecycle.py` 版を実行します。tests 自身がさらに private temporary chroots を作り、canonical `/nix/var/nix/gcroots` / queue / generation paths、real flock、unlink、SIGTERM、wrapper kill 後の実 subprocess FD 保持を検証します。systemd manager / worker status は明示的 test fixture であり、**booted NixOS の実 switch / shutdown integration proof ではありません**。host roots / daemon / consumer config は変更しません。native Nix sandbox や live Tailnet publication の検証を意味しません。

module の evaluation tests（host に適用しません）:

```sh
nix eval --json .#checks.x86_64-linux.nixos-module.passthru.results
nix build .#checks.x86_64-linux.nixos-module .#repro2-sender
```

Nix package の Cargo tests は `fakeroot` を test runner として実行します。Nix sandbox の `/` が unmapped UID 所有でも、root service 用の fixture を検証できるようにするためです。実際の root 権限は付与せず、permission / symlink の拒否テストも実行します。installed sender の所有者・権限検証は変更せず、runtime に `fakeroot` は使用しません。HTTP client の初期化は local HTTP fixture でも CA roots を読み込むため、`preCheck` で Nixpkgs `cacert` の store 内 bundle を `SSL_CERT_FILE` に明示します。host の `/etc/ssl/certs` はテストの前提にしません。NixOS service も独立して store 内 CA bundle を設定します。

### Manual setup once（非 NixOS 向け運用例、テストは host に適用しません）

**Linux、信頼する local root、通常の `/nix/store` と `/nix/var/nix`、user-owned tailnet builder device** の例です。root の Nix daemon が呼ぶ hook と resident worker を同じ UID / store で動かします。registry / file-server の migration、loopback service、Serve / ACL と gateway の `REQUIRED_USERS` / `BLOB_BASE_URL` は上記の通り設定しておいてください。

```sh
cargo build --release -p builder --bin repro2-sender
sudo install -D -m 0755 target/release/repro2-sender /usr/local/libexec/repro2-sender
sudo install -d -m 0700 -o root -g root /var/lib/repro2-sender /nix/var/nix/gcroots/repro2
sudo install -m 0755 examples/repro2-post-build-hook /etc/nix/repro2-post-build-hook
# Replace both Serve URL placeholders and confirm the installed Nix executable path first.
sudo install -m 0644 examples/repro2-sender.service /etc/systemd/system/repro2-sender.service
```

`/etc/nix/nix.conf` に追加（既存設定を保持）:

```ini
post-build-hook = /etc/nix/repro2-post-build-hook
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now repro2-sender.service
sudo systemctl restart nix-daemon.service
# From now on: normal builds; no manual publication command.
nix build --no-link '.#your-package'
journalctl -u repro2-sender.service
```

`examples/repro2-post-build-hook` は binary の load / launch failure や signal による nonzero exit でも **CRITICAL warning を出して 0 を返す** fail-safe wrapper です。binary の `hook` 自身も local-record failure を明示して 0 を返します。**disk full / permissions / broken config / hook 自身の crash で記録できなかった build は、publication と GC retention を保証できません。** build success と local-record durability の両方を、故障した disk 上で保証することはできません。hook の stderr は Nix daemon / build logs、worker の errors は journal で監視し、local-record CRITICAL は必ず復旧 / 再 build してください。network failure は hook に到達せず build を失敗させません。

### Configuration / debug CLI

| Flag | Environment | Default / meaning |
| --- | --- | --- |
| `--spool` | `REPRO2_SPOOL` | `/var/lib/repro2-sender`, existing private directory |
| `--gc-roots` | `REPRO2_GC_ROOTS` | `/nix/var/nix/gcroots/repro2`, existing private directory |
| `run --registry-url` | `REPRO2_REGISTRY_URL` | required trusted Serve base URL |
| `run --blob-url` | `REPRO2_BLOB_URL` | required trusted Serve base URL; same backend as gateway BLOB_BASE_URL |
| `run --nix` | `REPRO2_NIX` | `nix`; service example uses an explicit executable |
| `run --store` | `REPRO2_STORE` | `auto`; must be the store whose hook recorded the job |
| `run --once` | — | debug scan, bypasses backoff; delivery errors return nonzero |

```sh
# Debug only. Normal operation is the resident service above.
repro2-sender enqueue /nix/store/<drv-hash>-example.drv /nix/store/<hash>-example
repro2-sender run --once --registry-url https://registry-host.example-tailnet.ts.net \
  --blob-url https://file-server-host.example-tailnet.ts.net
```

### Operational limits / explicit trust

- Each deployment handles **one configured Nix store**, not automatic discovery of arbitrary remote / per-build stores. A private directory elsewhere is **not automatically a GC root**: place the configured root directory under that store's actual `NIX_STATE_DIR/gcroots` (default `/nix/var/nix/gcroots`). For `local?root=/scratch/store`, use `/scratch/store/nix/var/nix/gcroots/repro2` and set the worker's same `--store`. The worker validates directory safety, not daemon configuration. The legacy builder's per-invocation isolated stores are not automatically routed into this system-store queue.
- root / host operators / same UID processes, Nix metadata and stored paths, local filesystem with working fsync / atomic rename / locks, authenticated IdP / Serve / tailnet ACL, registry / file-server durability, and gateway operator are **trusted**. This is not a sandbox against malicious local root. NFS / network spool filesystems are unsupported. All queue / root ancestors must be real, owned by root or service UID, and not group/world writable; queue/root directories must be service-owned 0700. No TEE, new identity proxy, independent signing, or NAR gateway proxy is introduced.
- **Copied / substituted dependency paths never cast reports.** Their bytes are uploaded for closure delivery, but each dependency still needs its own qualifying authenticated reports or an existing trusted substituter. A built output may have valid narinfo while full `nix copy` is blocked by a reference without per-path votes. The real-Nix integration test demonstrates both a successful reference-free import and that correctly blocked dependency closure. No artificial dependency votes or inferred independent builds are added.
- Nix's hook runs for executed builds, not every substitution. The sender accepts the legacy full-store-path JSON map and Nix 2.34.8's version-4 `derivations` envelope with exact basename keys and explicit output `path` fields. Unsupported versions, ambiguous paths and pathless output specifications remain queued with GC roots; paths are never inferred from another derivation or `env`. In v4 this includes fixed-CA `{method, hash}` outputs: their paths are computable by Nix, but this sender does not compute them. Floating CA / deferred / impure / unresolved dynamic outputs are also not claimed supported. Votes identify reporters, not a proof of independent execution. Existing builder settings allowing substitution remain unchanged.
- Jobs / retained store closures / crash-left temporary NAR files can consume disk while offline. There is no queue quota, bandwidth limiter, concurrent sender pool, poison-job drop, automatic broken-record repair, or blob GC. Delivery is serial; temporary NAR disk usage may be one full uncompressed path plus crash leftovers. Fix persistent 401 / 413 / malformed metadata / configuration failures rather than discarding retention. Stop the worker before operator repair. Partial cleanup / pre-manifest crash directories may need operator removal after confirming delivery or absence of a committed job.
- File-server first synchronizes file bytes and directory publication before acknowledgement; registry commit and immutable backend readback are trusted. Actual process restart tests pass, but **power-cut / storage-controller fault durability is not empirically verified**. Backups / remote storage persistence are operational responsibilities. Disk mutation / bit rot after acceptance remain outside this transport guarantee.
- The gateway still returns **unsigned IA narinfo without CA declarations**. Trusting this gateway does not create a Nix signing key. Any client setting that permits unsigned import must be an explicit trusted-root deployment decision (the isolated smoke uses only command-scoped `require-sigs=false`), not a global security bypass silently installed by this project. Live Serve / systemd / daemon setup is an operator action, not performed by tests.

## 検証

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
python3 tests/http_slice.py
python3 tests/blob_slice.py
python3 tests/sender_slice.py
# Opt-in: actual Nix executable; no installation/host daemon changes.
REPRO2_REAL_NIX=/absolute/path/to/nix python3 tests/nix_sender_slice.py
```

`tests/http_slice.py` は実際の migration / registry / gateway バイナリ、SQLite、一時的な local HTTP upstream を使います。3000 / 3001 が使用中なら実行しません。identity 必須、URL 検証、legacy 除外、再投稿 dedup、同一ユーザーの別結果、N=2、未公開ユーザーの一致票、不一致・同票、上流 NAR URL を確認します。ヘッダーは local trusted proxy を模して test が注入します。

`tests/blob_slice.py` は実 migration / SQLite / registry / gateway / file-server を起動し、N=2、未公開票、同一 IA path の複数 NAR 候補、missing artifact / blob の 404、直接 GET / HEAD、download SHA256、references / deriver の basename serialization、gateway restart の安定性を検証します。

`tests/sender_slice.py` は実 resident バイナリと上記 service 群を使い、local hook、503 と永続 backoff、SIGTERM / restart、後から到着した job の自動処理、blob / registry readback、GC root 解放、N=2 が再送で増えないことを検証します。Nix command は明示的 fake fixture、identity は **test-only local proxy** が注入します。通常 worker に identity 注入 option はありません。

`tests/nix_sender_slice.py` は **実 Nix 2.24.11** を extracted closure + local launcher で動かし、scratch 内の diverted local store に multioutput derivation を実 build しました。Nix 自身による hook 呼出、実 GC 中の drv / 全 outputs 保持、実 path-info / dump-path / references / deriver、SIGKILL / restart 後の自動再送、gateway からの `nix copy` による reference-free output の import と内容 / hash、一時 root 解放後の実 GC を検証しています。host Bash builder が isolated store の physical prefix に書き込みます。host Nix installation / daemon / config / services は変更していません。

**実 tailnet / Serve による IdP 検証、独立ユーザーの実再ビルド、署名 trust の検証は未実施です。** 実 Nix smoke は意図的に N=1 と test-only identity proxy を使い、unsigned gateway を信頼する consumer の `require-sigs=false` を command に限定して指定します。N=2 の policy は別 HTTP test で検証しています。blob / HTTP tests の wire-encoded NAR と legacy upstream mock は実 Nix の成果ではありません。

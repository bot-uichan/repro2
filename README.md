# repro2: Tailscale IdP による最小 report / threshold スライス

このブランチは `feat/regsitry-consensus` を土台にした最小実装です。registry は報告の保存・提供だけを担当し、採用ポリシーは gateway が担当します。TEE、複数 gateway、consensus daemon、UI はありません。

## 認証と信頼境界

registry の `POST /build-reports` は、信頼された Tailscale Serve が付与する `Tailscale-User-Login` をユーザー識別子として保存します。ヘッダーがない、空、空白のみ、テキストでない場合は **401**。JSON に `user_id` を指定しても認証には使いません。未認証で受理する本番用 fallback はありません。

**registry backend は `127.0.0.1:3001` にのみ bind します。この制約を外さないでください。** 任意のクライアントが直接 backend に接続できると、ヘッダーを偽装できます。同じホストのプロセス・運用者も信頼境界内です。代わりの local reverse proxy を使う場合も、外部から渡された identity ヘッダーを破棄し、認証済みの値だけを設定する必要があります。

[Tailscale Serve の公式 identity headers 説明](https://tailscale.com/docs/features/tailscale-serve#identity-headers) に従い、ユーザー所有の端末から **Serve の URL** に投稿してください。tagged devices はこのヘッダー方式ではユーザー投票者として未対応です。Funnel は使いません。ユーザーが複数端末を持っていても、同じ login は同じユーザーとして扱います。Serve / tailnet のアクセス許可と gateway 運用者は信頼する前提です。

## 報告と gateway ポリシー

報告フィールド:

- `drv_path`: nullable string
- `output_name`, `store_path_hash`, `store_path`, `nar_hash`: string
- `nar_size`: signed integer（gateway が負数・不正な Nix metadata を除外）
- `cache_url`: optional HTTP(S) cache base URL。認証情報・query・fragment は不可

registry は `(user_id, drv_path, output_name, store_path_hash, store_path, nar_hash, nar_size)` に SQLite unique index を持ち、同一ユーザー・同一結果の再投稿を upsert します。nullable 入力の NULL と空文字は区別します。cache URL は投票のキーではなく、再投稿時に最後の値（NULL を含む）で置き換えます。別の結果は別の報告として保存されます。既存行は migration 後も `user_id = NULL` のまま保持し、架空の所有者を補いません。

`GET /nar-info/{store_path_hash}` は identity・入力・結果・nullable cache URL を含む **全報告**を返します。未公開の報告も投票の材料として返すため、以前の cache URL 非 NULL のフィルタはありません。この endpoint は registry 自身が合意を判定するものではありません。

gateway は環境変数 **`REQUIRED_USERS=N`** を必須とします。未設定・0・負数・非整数・範囲外は listen 前に起動エラーです。

- 同じ drv/output と store path/hash・NAR hash/size に対し、**N 人の異なる認証済みユーザー**が一致すると採用可能です。端末数・レコード数では数えません。
- 同じユーザーが同じ結果を何回報告しても、その結果への投票は 1 票です。他の結果にも報告していても、その結果で 2 票にはなりません。
- 所有者なし・空 identity・不正 metadata は票にしません。store path と hash の整合性、要求された hash、上流 narinfo の path/hash/size も確認します。
- 未公開のユーザー報告も一致票になります。ただし採用する結果には、少なくとも 1 人の認証済みユーザーの cache URL が必要です。
- しきい値未達・公開 cache 不在は **404**。別ユーザーの不一致は停止条件ではなく、同票数による **409** ポリシーもありません。

**実装上の選択詳細（ユーザーが承認した新しいポリシーではありません）:** 複数結果がしきい値に達した場合は `(drv_path, output_name, store_path_hash, store path basename, canonical SRI NAR hash, numeric NAR size, cache_url)` の昇順で最初の公開・所有者付き報告を選びます。nullable 入力は NULL が先です。票数最大の結果を選ぶ仕組みではありません。選んだ cache の不在・不整合時に他の候補へ自動 failover はしません。

gateway は **narinfo だけ**を提供し、NAR URL は上流 cache の直接 URL にします。上流の署名などを維持し、独立署名や NAR proxy は追加しません。Nix クライアント側の署名検証設定は別途必要です。投稿可能ユーザーが選べる cache URL への outbound 接続は可能なので、信頼する tailnet 参加者・cache とネットワーク上の egress 制御を前提とします。このスライスは SSRF 対策用の cache allowlist を実装していません。

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

## 検証

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
python3 tests/http_slice.py
```

`tests/http_slice.py` は実際の migration / registry / gateway バイナリ、SQLite、一時的な local HTTP upstream を使います。3000 / 3001 が使用中なら実行しません。identity 必須、URL 検証、legacy 除外、再投稿 dedup、同一ユーザーの別結果、N=2、未公開ユーザーの一致票、不一致・同票、上流 NAR URL を確認します。ヘッダーは local trusted proxy を模して test が注入します。

**実 tailnet / Serve による IdP 検証、実 Nix ビルド・NAR 配信・署名検証は未実施です。上流 narinfo は明示的な mock であり、実 Nix の成功を装うものではありません。**

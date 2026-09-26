# ローカルキャッシュのデモ

`.#demo` を Builder の一時 Store でビルドし、静的バイナリキャッシュへコピーします。Registry は報告を保存し、Gateway はその narinfo を返します。コマンドはリポジトリのルートで実行してください。手順は `Justfile` にまとめています。

1. 別々の端末で Registry と Gateway を起動します。Registry は既存の `db.sqlite` を使います。

   ```nu
   just registry
   just gateway
   ```

ビルドから取得までを一度に実行し、実行中のコマンドと結果を段階ごとに表示するには、Registry・Gateway に加えて `just serve-cache` も別の端末で起動し、リポジトリのルートで次を実行します。

```nu
nu run-demo.nu
```

以下は同じ操作を個別に実行する手順です。

2. Builder を実行します。表示された output の store path を次の手順で使います。

   ```nu
   just build-demo
   ```

   Builder は `/tmp/repro2-demo-cache` に `nix copy` してから、配信先 URL 付きの報告を Registry に送ります。公開先は `Justfile` の `cache_dir` と `cache_url` で変更できます。

3. 静的キャッシュを別の端末で配信します。

   ```nu
   just serve-cache
   ```

4. 新しい空の Store で、Gateway だけを substituter にして取得します。`output_path` には手順 2 で表示された `/nix/store/...-repro2-cache-demo` を指定します。

   ```nu
   just demo-client /nix/store/手順2で表示されたパス-repro2-cache-demo
   ```

Gateway には `<store path hash>.narinfo` の要求が届き、NAR 本体は手順 3 の HTTP サーバーから取得されます。署名検証を無効にする設定は、この空のデモ用 Store に対するコマンドだけに指定します。

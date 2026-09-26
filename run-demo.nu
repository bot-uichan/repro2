#!/usr/bin/env nu

# README の registry / gateway / serve-cache を起動してから実行する。
def main [] {
    print "╭─ 1/3 Builder: デモをビルドし、キャッシュへコピー・Registry へ登録"
    print "│ 実行: just build-demo"
    let build = (^just build-demo | complete)
    if ($build.stdout | str trim) != "" {
        print $build.stdout
    }
    if ($build.stderr | str trim) != "" {
        print -e $build.stderr
    }
    if $build.exit_code != 0 {
        error make {msg: $"just build-demo が失敗しました (終了コード: ($build.exit_code))"}
    }

    let registered = ($build.stdout | parse -r 'Registered: (?P<path>/nix/store/[^\s]+)')
    if ($registered | is-empty) {
        error make {msg: "Builder の出力から登録済み store path を取得できませんでした"}
    }
    let output_path = ($registered | get 0.path)
    print $"╰─ 登録済み: ($output_path)"
    print ""

    let store_hash = ($output_path | str replace '/nix/store/' '' | split row '-' | first)
    let narinfo_url = $"http://127.0.0.1:3000/($store_hash).narinfo"
    print "╭─ 2/3 Gateway: narinfo を直接取得"
    print $"│ 実行: curl -fS ($narinfo_url)"
    let narinfo = (^curl -fS $narinfo_url | complete)
    if ($narinfo.stdout | str trim) != "" {
        print $narinfo.stdout
    }
    if ($narinfo.stderr | str trim) != "" {
        print -e $narinfo.stderr
    }
    if $narinfo.exit_code != 0 {
        error make {msg: $"narinfo の取得に失敗しました (終了コード: ($narinfo.exit_code))"}
    }
    print "╰─ narinfo 取得完了"
    print ""

    print "╭─ 3/3 Client: Gateway 経由で空の Store にキャッシュを取得"
    print $"│ 実行: just demo-client ($output_path)"
    ^just demo-client $output_path
    if $env.LAST_EXIT_CODE != 0 {
        error make {msg: $"just demo-client が失敗しました (終了コード: ($env.LAST_EXIT_CODE))"}
    }
    print $"╰─ 取得完了: ($output_path)"
}

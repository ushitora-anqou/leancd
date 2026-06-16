# Lean CD (leancd) 設計ドキュメント

- ステータス: Draft v0.1
- 作成日: 2026-06-13
- 対象: leancd `0.1.0`
- 成果物の位置づけ: 本ドキュメントは leancd の実装に先立つ設計（要件・アーキテクチャ・メモリ戦略・コンポーネント・技術選定・ベンチマーク計画）をまとめたものである。コード実装は本ドキュメントの承認後に着手する。

---

## 1. 概要

Lean CD（以下 **leancd**）は、Kubernetes クラスタ向けの Continuous Delivery（CD）ツールである。Rust で実装され、単一バイナリを Kubernetes の `Deployment` としてデプロイし、Kubernetes コントローラとして振る舞う。

Argo CD や Flux CD と同様に、Git レポジトリで管理される Kubernetes マニフェストを読み取り、自身が動作するクラスタに適用する。Git 上の変更を差分検知して反映し、クラスタ側で生じた乖離（drift）を検知して自動復旧する。また、手動同期と状況確認を行う CLI、および Prometheus 形式のメトリクスを備える。

### 1.1 設計の最優先目標

> **実プロセスの RSS（Resident Set Size）を常時 100MiB 以下に保つこと。**

これは本ソフトウェアにおける**史上命題**であり、他のあらゆる要件・利便性よりも優先される。機能性、実装の網羅性、リアルタイム性などを犠牲にしてでも、この制約を満たすことを最優先とする。すべての設計判断は「これが RSS を増やすか」を基準に評価する。

### 1.2 トレードオフの原則

RSS 100MiB という制約を満たすため、以下の原則でトレードオフを解く。

1. **キャッシュしない**: クラスタ全体のリソースをメモリにキャッシュしない。必要な時に必要な分だけ API から取得する。
2. **状態を持たない**: プロセス内の可変状態を最小化する。状態は可能な限り Kubernetes 上の小さなリソース（ConfigMap 1つと管理用 label）に委ねる。
3. **機能を絞る**: Argo CD / Flux CD が持つ機能のうち、RSS を増やす要因となるものは削る（後述のスコープ外を参照）。
4. **単一レポジトリ・引数駆動**: 複数の同期単位を管理する CRD 等の状態管理オブジェクトを持たず、1プロセス＝1 Git レポジトリ（指定パス）とする。全設定はコマンドライン引数で与える。

### 1.3 類似ツールとの機能比較

| 機能 | Argo CD | Flux CD | **leancd** |
|---|:---:|:---:|:---:|
| Git マニフェスト適用 | ✓ | ✓ | ✓ |
| Git 変更差分検知 | ✓ | ✓ | ✓（ポーリング） |
| クラスタ乖離の自動復旧 | ✓ | ✓ | ✓ |
| CLI | ✓ | ✓ | ✓ |
| Prometheus メトリクス | ✓ | ✓ | ✓ |
| Web UI | ✓ | ✗ | **✗** |
| Kustomize / Helm / Jsonnet | ✓ | ✓ | **✗** |
| Owner reference を辿る監視 | ✓ | ✓ | **✗** |
| 通知（Slack 等） | ✓ | ✓ | **✗** |
| 複数 Application / 同期単位 | ✓ | ✓ | **✗**（単一レポジトリ） |
| 典型的な RSS | 数百 MiB〜GiB | 数十〜百 MiB 超 | **目標 ≤ 100MiB** |

---

## 2. 要件

### 2.1 機能要件（必須）

1. **マニフェスト読取と適用**: Git レポジトリに配置された Kubernetes マニフェスト（YAML）を読み取り、自身が動作するクラスタに適用する。マニフェストは YAML のまま適用し、テンプレート展開（Kustomize/Helm/Jsonnet）は行わない。
2. **Git 変更の差分検知**: Git レポジトリの変更を検知し、変更されたマニフェストを適用する。変更検知は**ポーリング**による（定期的に `git fetch` し、HEAD commit の変化で判定する）。Git プロバイダからの Webhook 受信用 HTTP サーバは持たない。
3. **乖離の検知と自動復旧**: クラスタ上でマニフェスト外の変更が加えられ、Git レポジトリのマニフェストから状態が乖離した場合、これを検知し自動的にマニフェスト状態へ復旧する。監視対象は**Git マニフェストが直接指すリソースのみ**とし、Owner reference を辿った子孫リソース（例: `Deployment` が作る `ReplicaSet`/`Pod`）の監視は行わない。
4. **CLI**: CLI ツールを備え、以下を行える。
   - Git レポジトリとクラスタの同期を**手動でトリガー**する（`sync` サブコマンド）。`--force` オプションで force-conflict 適用を有効化できる。
   - 現在のデプロイ状況（同期状態・drift の有無・リソース数・最終同期時刻等）を**確認**する（`status` サブコマンド）。
5. **Prometheus メトリクス**: 現在デプロイされているソフトウェアの状態を表すメトリクスを Prometheus 形式で出力し、Grafana 等で監視できるようにする（`/metrics` エンドポイント）。

### 2.2 非機能要件

1. **RSS ≤ 100MiB（最重要）**: 実プロセスの RSS を常時 100MiB 以下に保つ。安定状態（アイドル）のみならず、同期実行中のピーク時も満たす。これを保証するためのベンチマーク環境とテストを実装する（第8章）。
2. **単一バイナリ・`Deployment` デプロイ**: 1つのバイナリとしてビルドされ、Kubernetes の `Deployment` でデプロイされる単一プロセスで動作する。CLI も同一バイナリのサブコマンドとして提供する。
3. **Rust 実装**: Rust で実装する。
4. **簡素な実装**: 機能を絞り、実装を簡素にする。これによりコード量・依存・CPU・メモリ使用率を下げる。

### 2.3 確定仕様（ヒアリング結果）

以下は設計に先立ち確定した仕様である。

- **Git 変更検知**: ポーリングのみ。Webhook 受信用 HTTP サーバは持たない。
- **同期セマンティクス**: 通常（自動同期・`sync` 既定）は **Apply + Prune**。`sync` サブコマンドに `--force` を渡した場合のみ **force-conflict 適用**（server-side apply の `force-conflicts`）を有効化する。`--force` は常時ではなく手動同期時のオプション指定時のみ。
- **リソース範囲**: **フル**。Kubernetes 標準リソースに加え、インストール済み CRD のカスタムリソース、クラスタスコープリソース（`Namespace`/`ClusterRole` 等）、`Secret` 等の機密リソースを含むすべてのリソースを扱う。
- **管理モデル**: **単一レポジトリ**。1プロセスは1つの Git レポジトリ（の指定パス）を扱う。すべての設定は**コマンドライン引数**で渡す。**機微情報（認証情報等）のみ** Secret から読み込む。Argo CD の `Application` のような、複数同期単位を管理する CRD は持たない。

### 2.4 スコープ外（明示的に対応しない）

以下は類似ツールがサポートするが、leancd では**サポートしない**。実装の簡素化と RSS 削減のためである。

- Kustomize / Helm / Jsonnet によるテンプレート展開。Git に置かれた YAML マニフェストをそのまま適用する。
- Owner reference を辿った子孫リソースの状態監視。Git マニフェストが直接指すリソースのみを監視する。
- Slack 等への通知/アラート。
- Web UI。
- 複数レポジトリ・複数同期単位（Application CRD 等）の管理。
- Git プロバイダからの Webhook 受信。

### 2.5 前提条件

- 対象クラスタは leancd が動作する単一クラスタ（in-cluster）。他クラスタへのデプロイは想定しない。
- leancd は十分な権限を持つ `ServiceAccount` で動作する（フルリソースを扱うため、対象リソースに対する get/list/watch/create/update/patch/delete 権限が必要）。
- Git レポジトリは SSH または HTTPS でアクセス可能。認証情報は Secret から注入する。
- メモリ測定は Linux の `/proc/[pid]/status` の `VmRSS` をもって RSS とする。

---

## 3. 全体アーキテクチャ

### 3.1 単一バイナリとサブコマンド

leancd は1つのバイナリであり、`clap` によるサブコマンドで動作モードを切り替える。

```
leancd controller [flags]      # Deployment で常駐するコントローラプロセス
leancd sync    [--force] [flags] # reconciliation を1回だけ実行（手動同期）
leancd status  [flags]          # 同期状況を表示
```

- **`controller`**: `Deployment` でデプロイされ、ポーリングによる定期 reconciliation ループを回す常駐プロセス。
- **`sync`**: reconciliation エンジンを**1回だけ**実行する。`controller` と**同一のエンジン**を共有する（定期ループか1回実行かの差のみ）。`--force` は force-conflict 適用のフラグとしてエンジンへ伝搬する。
- **`status`**: 同期状態を読み取って表示する（読み取り専用）。

### 3.2 `controller` と `sync` のエンジン共有

`controller` と `sync` は**同じ reconciliation エンジン**を使用する。これにより:

- 手動同期（`sync`）と自動同期（`controller`）の**一貫性**が保証される（適用ロジックが1つに集約される）。
- コード重複がない。

`controller` は「`sync` 相当の処理をポーリング間隔で繰り返す」実装になる。`sync` は「`controller` の1イテレーションだけを実行して終了する」実装になる。

CRD を介したコントローラへの指示出し（例: 同期要求 CRD の作成）は行わない。これは状態管理オブジェクトを持たないという方針（第2.3節）に合致し、追加の Watch・キャッシュを不要にして RSS を抑える。

### 3.3 reconciliation のデータフロー

1回の reconciliation（同期）の流れを以下に示す。

```
Git レポジトリ ──(shallow fetch)──▶ working tree
                                         │
                            (ストリーミング YAML パース)
                                         ▼
                         マニフェスト群（GVK + ns/name + spec）
                                         │
                    ┌────────────────────┼────────────────────┐
                    ▼                    ▼                    ▼
            API discovery          SSA 適用             Prune 判定
        (GVK → ApiResource)    (DynamicObject,        (管理 label 付きで
         CRD も解決)            fieldManager=leancd,   Git に無いものを削除)
                                --force で force)
                    │                    │                    │
                    └────────────────────┼────────────────────┘
                                         ▼
                          Drift 定期検査（次サイクル）
                          + 状態/メトリクス更新
```

- **Git fetch**: gix で shallow fetch。前回の HEAD commit SHA と比較し、変化がなければ以降の重い処理をスキップする（省電力・省メモリ）。
- **パース**: working tree の指定パス以下の YAML を 1 ドキュメントずつストリーミングパースする。
- **API discovery**: 各マニフェストの `apiVersion`/`kind`（GVK）を `ApiResource` に解決する。CRD も含む。結果はプロセス内に軽量キャッシュする（メタデータのみ）。
- **適用**: `DynamicObject` で各リソースを server-side apply（SSA）。`fieldManager=leancd`。`--force` 時は `force-conflicts`。
- **Prune**: leancd 管理ラベルを持つリソースのうち、Git マニフェストに存在しないものを削除する。
- **Drift 検査**: 次サイクル以降、対象 GVK ごとに `List` を取得し、Git 期待値と比較する。
- **状態/メトリクス更新**: 同期結果を ConfigMap に書き込み、メトリクスを更新する。

### 3.4 並行性と競合の扱い

- `controller`（常駐）と `sync`（手動、別 Pod/プロセス）が同時に実行される可能性がある。
- 適用は SSA であるため、同一 `fieldManager` での二重適用は冪等であり安全。
- Prune は Git HEAD に基づき管理 label で判定するため、両者が同じ HEAD を見る限り一貫する。競合時の安全性は SSA と label スコープで担保する。

---

## 4. メモリ戦略（RSS ≤ 100MiB の達成）

本章が本設計の核心である。RSS 100MiB 達成のための具体的施策を示す。

### 4.1 kube-rs のキャッシュ機構を使わない

kube-rs の `Controller` runtime と `Store`（informer キャッシュ）は、Watch 対象のリソースをすべてメモリにキャッシュする。これは大規模クラスタで数百 MiB を消費し得る。leancd ではこれらを**使わず**、必要な時に限り `List`/`Get` を直接呼ぶ。クラスタ全体のインメモリキャッシュは一切持たない。

### 4.2 Drift 検知は定期 `List` 比較（Watch しない）

drift 検知に `Watch`（常時接続・ストリーミング・キャッシュ）を使わず、**定期 `List` 比較**を採用する。

- 監視対象は「Git マニフェストが直接指すリソースのみ」（Owner reference 追跡なし、要件 第2.1.3項）。
- 各 GVK ごとに **1 回の `List`** を発行し、その中から対象 `name` のリソースの `spec` を取り出し、Git 期待値と比較する。
- API 呼び出し数は「対象 GVK 種類数」に抑えられ、定数接続を維持しないためメモリ・コネクションともに最小になる。
- 即時性はポーリング間隔分だけ遅れるが、トレードオフとして許容する（第1.2節の原則）。

### 4.3 Git shallow clone

git CLI（`git clone --depth 1` / `git fetch --depth 1`）で shallow に行う。履歴オブジェクトをメモリ/ディスクに載せず、working tree の現在状態のみを扱う。HEAD commit SHA の比較だけで差分有無を判定するため、履歴は不要である。

### 4.4 YAML のストリーミングパース

マルチドキュメント YAML を**1 ドキュメントずつ**パース・処理する。マニフェスト群全体を一度にメモリに展開しない。serde_yml のドキュメント区切りを利用し、1ドキュメントを処理し終えたら即座に次へ進む。

### 4.5 状態オブジェクトの最小化

- `Application` 等の同期単位を管理する CRD を持たない。
- プロセス内で保持する可変状態は「前回の HEAD commit SHA」「API discovery のメタデータ（軽量）」「実行中の調整状態」程度にとどめる。
- 永続状態は **ConfigMap 1つ**（同期サマリ）と、各リソースに付与する**管理用 label** のみ。独立した DB や大きなインデックスは持たない。

### 4.6 ランタイムのスレッド最小化

`tokio` を **`current_thread` ベース**で構成し、ワーカースレッド数を最小化してスレッド毎のスタックメモリを抑える。gix の fetch は本質的にブロッキングであるため `spawn_blocking` で処理する。

### 4.7 依存の最小化

- TLS は **rustls** 系を使用し、OpenSSL の動的リンクを避ける。
- クレートの `features` を必要最小限に絞る。
- 不要機能（Web UI・通知・Helm/Kustomize 等）に由来する依存を排除する。
- `cargo deny` で依存グラフを監視し、意図しない肥大化を検知する。

### 4.8 グローバルキャッシュを持たない

各 reconciliation は原則として都度計算し、プロセス全体で共有する巨大なキャッシュやインデックスを持たない。必要な中間データはその reconciliation のスコープ内で完結させ、処理終了とともに解放する。

---

## 5. コンポーネント設計

モジュール（クレート内のモジュール）ごとの責務と設計要点を示す。

### 5.1 `config`

- すべての設定を**コマンドライン引数**から読み取る。
- 主な引数: Git レポジトリ URL、対象ブランチ/ref、マニフェストのパス（Glob パターン、再帰対象、複数指定可）、ポーリング間隔、同期先 namespace、メトリクスリッスンアドレス、機微情報を格納した Secret の名前等。
- **機微情報**（Git 認証情報、HTTPS Basic 認証、SSH 秘密鍵等）は、引数ではなく**指定された Secret から**読み込む。Secret は環境変数またはボリュームマウントで注入する。
- 設定値のバリデーションを行う。

### 5.2 `git_sync`

- git CLI を用いて shallow fetch（`--depth 1`）を行う。
- 前回取得した HEAD commit SHA を保持し、新 SHA と比較して**差分有無**を判定する。
- git は `tokio::process::Command` で非同期に起動する（別プロセスで動くため leancd の RSS に含まれない）。
- working tree を指定パス以下で読み取れる状態にする。
- 認証は `config` が解決した機微情報（SSH/HTTPS）を用いる。

### 5.3 `manifest`

- serde_yaml により working tree の YAML を generic `Value` に**ストリーミングパース**する。
- 各ドキュメントから `apiVersion`/`kind`（GVK）、`metadata.namespace`、`metadata.name` を抽出する。
- マニフェストを型なし（generic）のまま保持し、`kube_apply` が `DynamicObject` のデータ部に変換できる形で渡す。
- List/Object の混在、`---` 区切りのマルチドキュメント、空ドキュメント等を適切に扱う。

### 5.4 `kube_apply`

- **API discovery** で GVK → `ApiResource` を解決する。CRD も含む。`kube::discovery` 系の機能を用い、結果は軽量にキャッシュする。
- `DynamicObject` を用いて任意のリソースを扱う。`Api::<DynamicObject>::namespaced_with` / `all_with` 等で API ハンドルを得る。
- **server-side apply（SSA）** で適用する。`fieldManager=leancd`。content-type は `application/apply-patch+yaml`。
- `--force` 指定時は `force-conflicts` を有効化する。
- 適用結果（作成/更新/変更なし/エラー）を記録し、メトリクス・状態に反映する。

### 5.5 `prune`

- leancd 管理ラベル（例: `app.kubernetes.io/managed-by=leancd`）を適用時に各リソースへ付与する。
- Prune では、**管理ラベル付きリソースのうち、現在の Git マニフェストに存在しないもの**を削除する。
- これにより「前回適用したリソース一覧」をプロセス内/ConfigMap に保持する必要がなく、状態依存を排除して RSS を抑える（label クエリで削除対象を特定）。
- 誤削除を防ぐため、削除対象は管理ラベルの有無でスコープを限定する。

### 5.6 `drift`

- 対象 GVK ごとに **1 回の `List`** を発行し、Git マニフェストが期待する `spec` と実際の `spec` を比較する。
- 差分があれば reconciliation（再適用）をトリガーする。
- `Watch` は使わない（第4.2節）。検知遅延はポーリング間隔で許容する。

### 5.7 `state`

- 同期状態（最終同期 commit SHA・時刻・同期結果・drift の有無・対象リソース数・直近のエラー）を **ConfigMap 1つ**（leancd が所有する固定名）に書き込む。
- `status` サブコマンドはこの ConfigMap を読み取って表示する。
- ConfigMap の 1MiB サイズ制限に収まるよう、サマリは簡潔に保つ（リソース毎の詳細は持たず、集計値と代表的なエラーのみ）。

### 5.8 `cli`

- `clap` の derive で `controller`/`sync`/`status` サブコマンドを定義する。
- 共通の引数（レポジトリ・パス・Secret 名等）は共有定義し、各サブコマンドで再利用する。
- `sync` は reconciliation エンジンを1回起動し、`--force` をエンジンへ伝搬する。
- `status` は ConfigMap を読み取って人間が読みやすい形式で出力する。

### 5.9 `metrics`

- Prometheus 形式で `/metrics` を公開する。最小の HTTP サーバ（`hyper` または `tiny_http`）でリッスンする。
- 出力メトリクス例:
  - `leancd_sync_total` / `leancd_sync_errors_total`（同期回数・エラー回数）
  - `leancd_sync_last_success_timestamp_seconds`（最終成功同期時刻）
  - `leancd_drift_detected`（drift 有無、GVK/discriminator ラベル付き）
  - `leancd_managed_resources`（管理対象リソース数）
  - `leancd_rss_bytes`（自身の RSS。自己監視）
- push 型のキュー等は持たず、pull（スクレイプ）専用とする。

### 5.10 `main` / エントリポイント

- `clap` でサブコマンドを解析し、対応するモジュールへディスパッチする。
- `tokio` ランタイムを `current_thread` ベースで構成する。
- ログは `tracing` で構造化出力する（バッファは最小）。
- シグナルハンドリング（`SIGTERM` でのグレースフルシャットダウン）を行う。

---

## 6. 技術選定と根拠

| 用途 | 選定 | 理由 / 代替案 |
|---|---|---|
| Kubernetes クライアント | `kube`（kube-rs）+ `k8s-openapi` | `DynamicObject` + `ApiResource` で CRD を含む全リソースを扱える。`Controller`/`Store` キャッシュを使わなければ軽量に運用できる。Rust で k8s を扱うための標準的選択。 |
| Git 操作 | `git` CLI（shell-out） | 別プロセスで動くため git のメモリが leancd の RSS に含まれず、史上命題（RSS 削減）に直結する。反復 fetch も枯れた `git` で確実。ベースイメージに `git` を含める。当初の設計案 `gix`（純 Rust）は、反復 fetch の低レベル API が複雑で実装リスクが高いため見送った（付録 B）。 |
| YAML パース | `serde_yaml` | マルチドキュメントのストリーミングパース（`Deserializer`）が安定している。`serde_yml` は v0.0.x で文字列からのストリーミング API を持たず要件を満たさなかったため見送った。`serde_yaml` は deprecated だが機能は安定し kube-rs も使用する（付録 B）。 |
| CLI | `clap`（derive） | サブコマンド・引数検証。標準的。 |
| メトリクス | `prometheus` クレート + 最小 HTTP サーバ | Prometheus 形式の pull 型出力。 |
| ログ | `tracing` + `tracing-subscriber` | 構造化ログ。バッファを最小化できる。 |
| 非同期ランタイム | `tokio`（`current_thread` ベース） | スレッド/スタックメモリの抑制。git は `tokio::process` で非同期起動（別プロセス）。 |
| RSS 計測（自己/テスト） | `procfs` クレート | `/proc/[pid]/status` の `VmRSS` を軽量に読み取る。 |

> **注記**: 各ライブラリの具体的な用法と実装での調整は **付録 B（実装ノート）** にまとめた。実装時は context7 で最新ドキュメントを確認しながら進めた（プロジェクト運用ルールに従う）。

---

## 7. データモデル・状態管理

### 7.1 プロセス内状態

プロセスがメモリに保持する可変状態は以下のみとする。

- **前回 HEAD commit SHA**: 差分検知用。数十バイト。
- **API discovery メタデータ**: GVK → `ApiResource` の対応表。メタデータのみで、リソース実体は含まない。規模に比例するが軽量。
- **実行中の調整状態**: 現在の reconciliation の進行状況。一時的。

### 7.2 永続状態（Kubernetes 上）

- **ConfigMap 1つ**: 同期サマリ（最終 commit・時刻・結果・drift 有無・リソース数・直近エラー）。leancd が所有する固定名。
- **管理ラベル**: 各管理対象リソースに付与する `app.kubernetes.io/managed-by=leancd` 等。

CRD・独立 DB・大きなインデックスは持たない。

### 7.3 リソースの識別と突合

- マニフェストは GVK（`apiVersion` + `kind`）+ namespace + name で一意に識別する。
- 適用時は GVK → `ApiResource` 解決 → `DynamicObject` で SSA。
- drift 検知・Prune は namespace/cluster スコープを区別し、それぞれの `Api` ハンドルを用いる。

---

## 8. ベンチマーク環境とテスト（RSS 保証）

RSS ≤ 100MiB を保証するため、実際的なクラスタを模擬したベンチマーク環境を構築し、そこで RSS を計測・検証するテストを実装する。

### 8.1 模擬クラスタ

- **軽量な本物 k8s** として `kind`（Kubernetes in Docker）または `k3s` を用いる。fake API server ではなく実際の API server + etcd を使うことで、Watch/List/SSA の実挙動を反映する。
- クラスタ上に、**数十〜数百のマニフェスト**（`Deployment`/`Service`/`ConfigMap`/`Secret`/`CRD` リソース等、クラスタスコープ・namespace スコープ混在）を置いた Git レポジトリ（ローカルまたは fake リモート）を用意する。

### 8.2 RSS 測定

- leancd をクラスタにデプロイ（またはローカルでクラスタに接続）し、同期を実行する。
- `procfs` クレート、または `/proc/[pid]/status` の `VmRSS` を直接読み取り、**定期サンプリング**する。
- 計測ポイント:
  - **安定状態（アイドル）**: 同期完了後、ポーリング待機中の RSS。
  - **同期ピーク**: fetch・パース・適用実行中の RSS 最大値。
- 両ポイントで **RSS < 100MiB を `assert`** する。

### 8.3 スケール追跡

- リソース数を段階的に増やし（例: 100 / 300 / 500 リソース）、RSS の推移を記録する。
- どの規模で 100MiB に近づくかを把握し、必要に応じてメモリ戦略（第4章）を強化する。

### 8.4 CI 統合

- ベンチマーク・RSS テストを `Makefile` ターゲット（`make bench` / `make scale`）で実行可能にする。
- RSS しきい値（100MiB）を超過した場合はスクリプトを**非0終了**とし、ジョブを落とす。これにより回帰を防ぐ。
- **実装上の制約**: ベンチマークは `kind`/Docker を必要とするため、sandbox 上の `nix flake check` には含めない。`make test`（= `nix flake check`）は静的チェック（fmt/clippy/nextest/deny/audit）のみとし、RSS 回帰検知は `make bench` / `make scale` を手動または外部 CI ジョブ（kind 利用可能環境）で実行することで担保する（付録B）。

### 8.5 テストの補助

- leancd 自身が `leancd_rss_bytes` メトリクスとして RSS を公開し、スクレイプで継続監視できるようにする。
- 単体テストでは、API 呼び出しを mock して reconciliation ロジック（パース・適用判定・Prune 対象抽出・drift 比較）を検証する。RSS 計測は模擬クラスタを用いる結合テストで行う。

---

## 9. 実装マイルストーン

以下の順序で実装を進める。各ステップでビルド・テストが通ることを確認してから次へ進む。

1. **scaffold・依存追加・CLI 骨格**: `Cargo.toml` への依存追加、`clap` による `controller`/`sync`/`status` サブコマンドの骨格、`config` モジュール。
2. **`git_sync`**: gix による shallow fetch・HEAD 変更検知。
3. **`manifest`**: serde_yml によるストリーミングパース・GVK/ns/name 抽出。
4. **`kube_apply`**: API discovery・`DynamicObject`・SSA（`--force` 対応）。
5. **`prune`**: 管理ラベルに基づく削除。
6. **`drift`**: 対象 GVK ごとの `List` 比較。
7. **`state` + `status` CLI**: ConfigMap への状態書込と `status` サブコマンド。
8. **`sync` CLI**: エンジンの1回実行・`--force` 伝搬。
9. **`metrics`**: Prometheus `/metrics`。
10. **ベンチマーク環境・RSS テスト**: 模擬クラスタ・RSS 計測・しきい値 assert。
11. **Nix/Makefile・CI 統合**: ビルド・テスト・ベンチマークの CI 化。
12. **`Deployment` マニフェスト・README**: デプロイ方法とドキュメント整備。

---

## 10. リスクと緩和策

- **kube-rs 依存の重さ**: トランスitive 依存（`tonic`/TLS 等）がバイナリサイズ・メモリを増す可能性。→ rustls 系を採用、`features` を最小化、`cargo deny` で監視。
- **gix のバイナリ/コンパイル肥大**: gix は大規模クレートでバイナリサイズ・コンパイル時間が増す。→ `features` を最小化、ベンチマークで影響を確認、必要なら `git2`/`git` CLI 代替を検討（第6章）。
- **リソース大量時の `List` API 負荷**: drift 検知の定期 `List` がリソース種類数に比例する。→ 実用規模（数百リソース）に抑える設計、ポーリング間隔で調整。スケール上限を明示する。
- **Prune の誤削除**: 管理ラベルの付与漏れや想定外リソースの削除。→ 削除対象を管理ラベルで厳密にスコープ、dry-run モード・確認ステップの用意。
- **SSA と既存リソースのコンフリクト**: 他の管理者が保持するフィールドとの競合。→ `fieldManager=leancd` で管理範囲を明示、`--force` で強制オプションを提供。
- **ConfigMap の 1MiB 制限**: 状態サマリの肥大化。→ サマリを集計値に最小化、必要なら複数 ConfigMap へ分割。
- **`current_thread` とブロッキング処理**: gix fetch 等のブロッキング処理がイベントループを塞ぐ。→ 一切のブロッキング処理を `spawn_blocking` に回す。
- **RSS 超過の回帰**: 実装の進展で RSS が増える可能性。→ 第8章の RSS テストを CI で必ず実行し、しきい値超過で即座に検知する。

---

## 付録 A: 用語

- **RSS（Resident Set Size）**: プロセスが物理メモリ上に保持しているメモリ量。本設計では `/proc/[pid]/status` の `VmRSS` をもってこれとする。
- **SSA（Server-Side Apply）**: Kubernetes の適用方式の一つ。サーバ側でフィールドの所有権を管理し、`fieldManager` 単位でマージ/競合解決を行う。
- **Drift**: クラスタ上の実際の状態が、Git マニフェスト（期待状態）から乖離していること。
- **Prune**: Git マニフェストから削除されたリソースを、クラスタからも削除する機能。
- **GVK（Group/Version/Kind）**: Kubernetes リソースの種別を表す（例: `apps/v1 Deployment`）。

---

## 付録 B: 実装ノート

設計（本編）からの主な具体化・調整点。

- **Git**: 設計では `gix` を既定としたが、実装では `git` CLI への shell-out（`tokio::process`）を採用した。(1) git が別プロセスで leancd の RSS に含まれない、(2) 反復 fetch の実装が確実、が理由。ベースイメージに `git` を含める。本編第3〜5章に残る「gix」「spawn_blocking」という表現は設計段階の検討記述であり、実装では `tokio::process` で非同期に `git` を起動する。
- **YAML**: `serde_yaml` を採用。`serde_yml` は v0.0.x で文字列からのストリーミングパース API を持たず、マルチドキュメント処理（`Deserializer`）に不適だった。
- **kube-rs v3**: `discovery::pinned_kind(client, &gvk)` が `(ApiResource, ApiCapabilities)` を返す。`ApiCapabilities.scope` で `Scope::Cluster` / `Scope::Namespaced` を判定する。適用は `Api::<DynamicObject>::patch(name, &PatchParams::apply(fm).force(), &Patch::Apply(&obj))`。
- **Drift 検知**: `Watch` ではなく、対象 GVK ごとに管理ラベルで `List` を 1 回発行し、Git 期待 spec と部分集合比較（`spec_subset`）で差分を検知する。k8s が注入するデフォルトは許容する。
- **Prune**: 状態 ConfigMap の「適用済みキー集合」と現在の Git 集合の差分を**主シグナル**として削除する。加えて安全網として、前回適用した各 GVK について管理ラベル付きのライブリソースを `List` し、Git に存在しないものを削除候補に追加する（適用済みキーが一部欠損しても孤立リソースを回収）。`prev` が完全に空（状態消失）の場合は安全網の対象 GVK が無いため回収できず、簡素さ・省メモリを優先して API discovery の全走査は行わない（§4 の原則）。本編 §5.5「適用済み一覧を保持する必要がない」は実装では「保持する」形に調整した（本項が正）。
- **状態**: 単一 ConfigMap（`leancd-state`）に同期サマリと適用済みキー集合を文字列で保持する。
- **ランタイム**: `tokio` current_thread。metrics は最小 HTTP サーバ（`tokio::net::TcpListener`）で `/metrics` を公開する。
- **ベンチマーク**: `bench/bench.sh` は起動から安定状態まで `leancd_rss_bytes` を定期サンプリング（既定30s、`BENCH_SAMPLE_SECS`）し、同期ピーク（最大値）とアイドル（最終値）の両方が予算内であることを検証する（§8.2 の両ポイント assert）。`bench/bench.sh` は複数 namespace × Deployment/StatefulSet/ConfigMap/Service の実環境寄りマニフェストを生成する。`bench/scale.sh` は 8/15/20 namespace（`SCALE_NS_LEVELS`）と規模を変えてピーク/アイドル RSS を集計し、スケールに対する推移を記録する（§8.3）。いずれも `kind` 必須のため `nix flake check` には含まず、外部 CI での実行を前提とする（§8.4）。2026-06 の改修で、`bench/bench.sh` は自己 RSS（`leancd_rss_bytes`）に加えて **プロセスツリー全体の RSS 合計**（leancd + git/ssh の全子孫プロセスを `ps` で外部観測して単純合計）も同時にサンプリングし、self/tree それぞれのピーク・アイドル（計4値）が予算内であることを検証するようになった（出力キー `treerss_max`/`treerss_min`）。共有ページが二重カウントされるため過大評価（保守的・安全側）である。§5.2/§8.2 の「git は別プロセスで leancd の RSS に含まれない」という意図は不変で、これは git のメモリも含めた上で 100MiB を守ることを確認する安全側の追加検証である。

### 探索的テスト後の修正（2026-06: leancd vs Argo CD 比較で発見）

`exploratory/` で Argo CD との比較探索を行い、以下の不具合を修正した（実装と単体/E2E テストを参照）。

- **状態 ConfigMap に管理ラベルを付与しない（BUG 2）**: 当初の実装では状態 ConfigMap に管理ラベルを付けていたため、prune の安全網が毎パスこれを削除していた。§5.5 / 上記 Prune 項のとおりラベルなしで生成し、安全網のラベルセレクタ `List` から除外されるようにした。
- **Drift の配列比較を部分集合に（BUG 3）**: `spec_subset` が配列を完全一致（`==`）で比較していたため、コンテナの `resources`/`imagePullPolicy`/`terminationMessage*`/`ports[].protocol` 等のサーバ注入デフォルトで毎パス drift と誤判定し、無限再適用ループになっていた。インデックス整列の再帰部分集合に変更し、サーバデフォルトを許容する（上記 Drift 検知項の趣旨）。
- **複数 namespace 管理（BUG 5）**: 当初 drift/prune の `List` を `LEANCD_NAMESPACE` のみに発行していた。apply は各マニフェストの namespace に従うため、これと整合させるべく `List` を全 namespace（`Api::all_with` + 管理ラベルセレクタ）に変更した。本編の「単一 namespace」想定は「管理対象は1 namespace とは限らない」に訂正する。
- **Drift 自己修復で force 適用（BUG 4）**: 本編 §3.3 では「`--force` は手動同期時のみ」としていたが、定期 controller の drift 再適用でも `force=true` で競合フィールドを Git 値へ復元するよう変更した（Argo CD の self-heal と同等）。初回の通常適用は引き続き controller の `force`（false）に従う。
- **Dockerfile のビルド再現性（BUG 1）**: 依存キャッシュ用のダミービルド後に `COPY src/` して `cargo build` した際、BuildKit がホストの（古い）mtime を保持し Cargo の mtime fingerprint が再コンパイルをスキップするため、ダミー `fn main(){}` が出荷されることがあった。`COPY src/` 後に `touch src/*.rs` して実ソースを確実に再コンパイルさせる。
- **spec なしリソースの drift 過検知（BUG 6）**: `compute_drifts` が Git マニフェスト（`apiVersion`/`kind` を含む）と `DynamicObject.data`（`#[serde(flatten)]` で `apiVersion`/`kind` が `types` 側に剥がれる）を直接比較していたため、ConfigMap 等 spec のないリソースが毎パス drift と判定されていた。live の `DynamicObject` を `serde_json::to_value` で再直列化してから `specs_differ` に渡すよう修正した（apiVersion/kind/metadata が含まれるため整合する）。

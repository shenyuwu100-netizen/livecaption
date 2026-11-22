import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type AudioStatus = {
  enabled: boolean;
  recognitionHealth: string;
  status: string;
  message: string;
  source: string;
};

type CaptionItem = {
  id: number;
  startMs: number;
  endMs: number;
  timestamp: string;
  text: string;
  decodeMs: number;
  continued: boolean;
};

type CaptionStatus = {
  recording: boolean;
  sessionId: string | null;
  captionCount: number;
  gapCount: number;
  avgDecodeMs: number;
  p50DecodeMs: number;
  p95DecodeMs: number;
  lastDecodeMs: number;
  message: string;
};

type SyncSnapshot = {
  enabled: boolean;
  connected: boolean;
  endpoint: string;
  revision: number;
  syncedCaptions: number;
  lastCaption: CaptionItem | null;
  message: string;
};

type SummaryVerification = {
  valid: boolean;
  citedIds: number[];
  missingCitationIds: number[];
  unsupportedNumbers: string[];
  uncitedLines: string[];
};

type GeneratedSummary = {
  summary: string;
  mode: string;
  gatewayName?: string | null;
  modelUsed?: string | null;
  attempts: number;
  verification: SummaryVerification;
};

type RouteConfig = {
  name: string;
  baseUrl: string;
};

type Source = { id: string; name: string };

export default function App() {
  const [audio, setAudio] = useState<AudioStatus | null>(null);
  const [captionStatus, setCaptionStatus] = useState<CaptionStatus | null>(null);
  const [captions, setCaptions] = useState<CaptionItem[]>([]);
  const [sync, setSync] = useState<SyncSnapshot | null>(null);
  const [language, setLanguage] = useState("zh");
  const [sources, setSources] = useState<Source[]>([]);
  const [source, setSource] = useState("");
  const [exportPath, setExportPath] = useState("D:\\CodexArtifacts\\livecaption-session.srt");
  const [searchQuery, setSearchQuery] = useState("");
  const [llmBaseUrl, setLlmBaseUrl] = useState("");
  const [llmApiKey, setLlmApiKey] = useState("");
  const [llmModel, setLlmModel] = useState("gpt-4o-mini");
  const [routesText, setRoutesText] = useState("");
  const [gatewayRoutes, setGatewayRoutes] = useState<RouteConfig[]>([]);
  const [summaryMeta, setSummaryMeta] = useState("");
  const [summaryDraft, setSummaryDraft] = useState("");
  const [verification, setVerification] = useState<SummaryVerification | null>(null);
  const [consentChecked, setConsentChecked] = useState(true);
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    const poll = async () => {
      try {
        const [a, c, list, s] = await Promise.all([
          invoke<AudioStatus>("get_audio_status"),
          invoke<CaptionStatus>("get_caption_status"),
          invoke<CaptionItem[]>("get_captions", { query: searchQuery.trim() || null }),
          invoke<SyncSnapshot>("get_sync_status"),
        ]);
        if (alive) {
          setAudio(a);
          setCaptionStatus(c);
          setCaptions(list);
          setSync(s);
        }
      } catch {
        // Ignore transient poll errors while shutting down
      }
    };
    void invoke<string>("get_audio_language").then((x) => {
      if (alive) setLanguage(x);
    });
    void invoke<string>("get_audio_source").then((x) => {
      if (alive) setSource(x);
    });
    void invoke<RouteConfig[]>("get_gateway_routes").then((x) => {
      if (alive) setGatewayRoutes(x);
    });
    void invoke<Source[]>("list_audio_sources")
      .then((x) => {
        if (alive) setSources(x);
      })
      .catch((e) => {
        if (alive) setNotice(String(e));
      });
    void poll();
    const timer = setInterval(() => void poll(), 1000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [searchQuery]);

  async function act(action: () => Promise<unknown>, message = "已完成") {
    setBusy(true);
    setNotice("");
    try {
      await action();
      setNotice(message);
    } catch (e) {
      setNotice(String(e));
    } finally {
      setBusy(false);
    }
  }

  const listening = !!audio?.enabled;

  return (
    <main className="control-shell">
      <header className="hero">
        <div>
          <p className="eyebrow">LIVECAPTION · LOCAL-FIRST ACCESSIBILITY SUBTITLES</p>
          <h1>LiveCaption 本地实时字幕工作台</h1>
          <p className="hero-copy">
            通过 Windows WASAPI 采集系统或指定应用播放音频，由本机常驻 whisper.cpp 实时转写为无障碍字幕，音频全程不离开本机。
          </p>
        </div>
        <div className="actions">
          <button
            disabled={busy}
            onClick={() => void act(() => invoke("hide_control"), "已最小化到系统托盘")}
          >
            最小化到托盘
          </button>
          <button
            disabled={busy}
            onClick={() => void act(() => invoke("quit_app"), "正在退出")}
          >
            退出程序
          </button>
        </div>
      </header>

      <section className="panel consent-banner">
        <label className="consent-check">
          <input
            type="checkbox"
            checked={consentChecked}
            onChange={(e) => setConsentChecked(e.target.checked)}
          />
          <span>
            <strong>合规与知情同意声明：</strong>
            仅在已获得参会者或课程讲师知情同意的无障碍辅助、跨语种讲座记录场景下开启实时字幕。
          </span>
        </label>
      </section>

      <section className="panel">
        <div className="panel-heading">
          <h2>WASAPI 播放音频采集与本地 ASR</h2>
          <span className={listening && audio?.status !== "error" ? "badge ok" : "badge neutral"}>
            {audio?.status === "error"
              ? "需要注意"
              : listening
                ? "实时字幕运行中"
                : "字幕已暂停"}
          </span>
        </div>
        <div className="grid-two">
          <label>
            播放音频来源
            <select value={source} onChange={(e) => setSource(e.target.value)}>
              {sources.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name}
                </option>
              ))}
            </select>
          </label>
          <label>
            识别语言
            <select value={language} onChange={(e) => setLanguage(e.target.value)}>
              <option value="zh">中文（推荐，常驻识别平均 ~1.7s）</option>
              <option value="en">英文</option>
              <option value="auto">中英混合自动检测</option>
            </select>
          </label>
        </div>
        <div className="actions">
          <button
            disabled={busy}
            onClick={() =>
              void act(async () => {
                await invoke("save_audio_source", { source });
                await invoke("save_audio_language", { language });
              }, "音源和语言已保存")
            }
          >
            保存音源与语言
          </button>
          <button
            disabled={busy}
            onClick={() =>
              void act(
                async () => setSources(await invoke<Source[]>("list_audio_sources")),
                "已刷新播放设备与应用进程",
              )
            }
          >
            刷新音源
          </button>
          <button
            className="primary"
            disabled={busy || (!listening && !consentChecked)}
            onClick={() =>
              void act(
                () => invoke("set_caption_listening", { enabled: !listening }),
                listening ? "已暂停实时字幕" : "已开启实时字幕",
              )
            }
          >
            {listening ? "暂停实时字幕" : "开始实时字幕"}
          </button>
        </div>
        <p className="hint" role="status">
          {audio?.message || "就绪"} · 版本号 rev={sync?.revision ?? 0} · 识别延迟 P50={" "}
          {captionStatus?.p50DecodeMs ?? 0} ms / P95={captionStatus?.p95DecodeMs ?? 0} ms（平均{" "}
          {captionStatus?.avgDecodeMs ?? 0} ms） · 跨端阅读页：{sync?.endpoint || "http://127.0.0.1:8788/captions"}
        </p>
      </section>

      <section className="panel">
        <div className="panel-heading">
          <h2>实时字幕流（共 {captionStatus?.captionCount ?? 0} 条）</h2>
          <div className="actions">
            <button
              disabled={busy || captions.length === 0}
              onClick={() =>
                void act(
                  async () => {
                    const lower = exportPath.toLowerCase();
                    const fmt = lower.endsWith(".txt")
                      ? "txt"
                      : lower.endsWith(".vtt")
                        ? "vtt"
                        : "srt";
                    const msg = await invoke<string>("export_captions", {
                      path: exportPath,
                      format: fmt,
                    });
                    setNotice(msg);
                  },
                  "字幕导出完成",
                )
              }
            >
              导出 .SRT / .VTT / .TXT
            </button>
            <button
              disabled={busy || captions.length === 0}
              onClick={() => void act(() => invoke("clear_captions"), "已清空当前会话字幕")}
            >
              清空字幕
            </button>
          </div>
        </div>
        <div className="grid-two">
          <label>
            导出路径（支持 .srt SubRip、.vtt WebVTT、.txt 时间戳逐字稿）
            <input value={exportPath} onChange={(e) => setExportPath(e.target.value)} />
          </label>
          <label>
            字幕检索（按关键词、时间或 #序号 过滤）
            <input
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="输入关键词或 #1 实时过滤..."
            />
          </label>
        </div>
        <div className="caption-feed" role="log" aria-live="polite">
          {captions.length === 0 ? (
            <p className="hint">
              {searchQuery.trim()
                ? "未找到匹配的字幕段落（检索会自动保留紧邻的“但/需核实”限定句）。"
                : "暂无字幕片段。开始播放音频并点击“开始实时字幕”后将在此处实时滚动显示。"}
            </p>
          ) : (
            captions.map((item) => (
              <div key={item.id} className="caption-row">
                <span className="caption-meta">
                  [#{item.id} · {item.timestamp} · {item.decodeMs} ms]
                </span>
                <span className="caption-text">{item.text}</span>
              </div>
            ))
          )}
        </div>
      </section>

      <details className="panel">
        <summary>会后纪要生成（NewAPI 多渠道流式容错）与引用/数字事实校验</summary>
        <p className="hint">
          支持直连单网关或导入多条 NewAPI 渠道池（按模型故障与 401/402 额度分级切换、三阶段流式超时、跨分片 UTF-8 SSE 解码）；未配置网关时自动走本机抽取式纪要兜底，并对每条要点自动执行 <code>[#序号]</code> 引用与数字事实核验。
        </p>
        <div className="grid-two">
          <label>
            网关 Base URL（可选，留空用已导入渠道池或本地抽取）
            <input
              value={llmBaseUrl}
              onChange={(e) => setLlmBaseUrl(e.target.value)}
              placeholder="例如 http://127.0.0.1:3000/v1 或 https://api.openai.com/v1"
            />
          </label>
          <label>
            API Key（仅驻留进程内存，不落盘）
            <input
              type="password"
              value={llmApiKey}
              onChange={(e) => setLlmApiKey(e.target.value)}
              placeholder="sk-...（本机无鉴权网关可留空）"
              autoComplete="off"
            />
          </label>
        </div>
        <label>
          首选模型名称（失败时自动按同渠道备用模型列表回退）
          <input
            value={llmModel}
            onChange={(e) => setLlmModel(e.target.value)}
            placeholder="gpt-4o-mini"
          />
        </label>
        <details>
          <summary>
            NewAPI 多渠道容错池（当前已载入 {gatewayRoutes.length} 条：
            {gatewayRoutes.map((r) => r.name).join(" → ") || "未导入"}）
          </summary>
          <textarea
            className="context-box"
            value={routesText}
            onChange={(e) => setRoutesText(e.target.value)}
            placeholder={'每行一条，例如：\nnewapi-local: {"url":"http://127.0.0.1:3000/v1","key":"sk-local"}\nnewapi-backup: {"url":"https://backup.example.com/v1","key":"sk-backup"}'}
          />
          <div className="actions">
            <button
              disabled={busy || !routesText.trim()}
              onClick={() =>
                void act(async () => {
                  const imported = await invoke<RouteConfig[]>("import_gateway_routes", {
                    rawRoutes: routesText,
                  });
                  setGatewayRoutes(imported);
                  setRoutesText("");
                }, "已载入 NewAPI 多渠道容错池（密钥仅存于内存）")
              }
            >
              导入多渠道容错池
            </button>
          </div>
        </details>
        <textarea
          className="context-box"
          value={summaryDraft}
          onChange={(e) => setSummaryDraft(e.target.value)}
          placeholder="示例：本季度字幕覆盖率达 96.4% [#1]，下周完成 3 场讲座测试 [#2]。"
        />
        <div className="actions">
          <button
            className="primary"
            disabled={busy || captions.length === 0}
            onClick={() =>
              void act(async () => {
                const res = await invoke<GeneratedSummary>("generate_meeting_summary", {
                  baseUrl: llmBaseUrl.trim() || null,
                  apiKey: llmApiKey.trim() || null,
                  model: llmModel.trim() || null,
                });
                setSummaryDraft(res.summary);
                setVerification(res.verification);
                setSummaryMeta(
                  res.mode === "llm_stream"
                    ? `流式生成完成：渠道 ${res.gatewayName || "-"} · 模型 ${res.modelUsed || "-"} · 共 ${res.attempts} 次尝试`
                    : "本地抽取式纪要兜底完成（未配置外部网关）",
                );
              }, "已生成会后纪要并完成引用与数字核验")
            }
          >
            一键生成并校验纪要
          </button>
          <button
            disabled={busy || !summaryDraft.trim()}
            onClick={() =>
              void act(async () => {
                const res = await invoke<SummaryVerification>("verify_meeting_summary", {
                  summary: summaryDraft,
                });
                setVerification(res);
              }, "已完成纪要引用与事实核验")
            }
          >
            仅校验当前草稿
          </button>
        </div>
        {summaryMeta && <p className="hint">{summaryMeta}</p>}
        {verification && (
          <div className={verification.valid ? "notice ok" : "notice warn"}>
            <strong>{verification.valid ? "校验通过" : "发现未证实引用或数字"}</strong>
            <div>已引用字幕段落：{verification.citedIds.map((id) => `#${id}`).join(", ") || "无"}</div>
            {verification.missingCitationIds.length > 0 && (
              <div>不存在的字幕引用：{verification.missingCitationIds.map((id) => `#${id}`).join(", ")}</div>
            )}
            {verification.unsupportedNumbers.length > 0 && (
              <div>原文字幕未出现的数字：{verification.unsupportedNumbers.join(", ")}</div>
            )}
            {verification.uncitedLines && verification.uncitedLines.length > 0 && (
              <div>缺少 [#序号] 引用的句子：{verification.uncitedLines.join(" ； ")}</div>
            )}
          </div>
        )}
      </details>

      {notice && (
        <div className="notice" role="status">
          {notice}
        </div>
      )}
    </main>
  );
}

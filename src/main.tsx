import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { tr } from "./i18n";

class ErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { error: string | null }
> {
  state = { error: null as string | null };

  static getDerivedStateFromError(error: unknown) {
    return { error: error instanceof Error ? error.message : String(error) };
  }

  componentDidCatch(error: unknown, info: React.ErrorInfo) {
    console.error("[launcher] unrecoverable UI render error", error, info.componentStack);
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <main style={{ minHeight: "100vh", display: "grid", placeItems: "center", padding: 32, background: "#0b0d12", color: "#f4f6fb" }}>
        <section style={{ width: "min(560px, 100%)", padding: 28, border: "1px solid #303642", borderRadius: 14, background: "#151922" }}>
          <h1 style={{ marginTop: 0 }}>{tr("ShardX Launcher could not display this page")}</h1>
          <p style={{ color: "#b7bfce", lineHeight: 1.6 }}>{tr("Your portable profiles and browser data were not changed. Reload the launcher UI to try again.")}</p>
          <pre className="small" style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere", color: "#ffb4b4" }}>{this.state.error}</pre>
          <button type="button" onClick={() => window.location.reload()} style={{ marginTop: 12, padding: "9px 16px", border: 0, borderRadius: 8, cursor: "pointer" }}>{tr("Reload")}</button>
        </section>
      </main>
    );
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);

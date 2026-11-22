import layout from "./table-layout.json";

export function applyTableLayout() {
  const tables = [["browser", "t-cols"], ["proxy", "p-cols"]] as const;
  const fixedWidth = layout.dragColumn + layout.actionsColumn + layout.tablePaddingStart + layout.tablePaddingEnd;
  const styles = tables.map(([name, grid]) => {
    const columns = [layout.selectColumn, ...Object.values(layout[name].columns)];
    const minimum = columns.reduce((sum, width) => sum + width, fixedWidth + (columns.length + 1) * layout.columnGap);
    return { name, grid, columns, minimum };
  });
  const style = document.createElement("style");
  style.textContent = `:root {
    --layout-sidebar-width: ${layout.sidebarWidth}px;
    --main-min-padding: ${layout.minimumSidePadding}px;
    --scrollbar-width: ${layout.scrollbarWidth}px;
    --table-border: ${layout.tableBorder}px;
    --column-gap: ${layout.columnGap}px;
    --table-drag-column: ${layout.dragColumn}px;
    --table-actions-column: ${layout.actionsColumn}px;
    --table-padding-start: ${layout.tablePaddingStart}px;
    --table-padding-end: ${layout.tablePaddingEnd}px;
    --table-min-width: ${Math.max(...styles.map(({ minimum }) => minimum))}px;
  }
  ${styles.map(({ name, grid, columns, minimum }) => {
    // Leave rounding residue in the last growing track so the fixed actions track stays flush.
    const tracks = columns.slice(0, -1).map(width => `calc(${width}px + var(--column-extra))`);
    return `.${name}-rows {
    --growing-columns: ${columns.length};
    --table-tracks-width: ${minimum}px;
  }
  .${grid} {
    grid-template-columns: var(--table-drag-column) ${tracks.join(" ")} minmax(${columns[columns.length - 1]}px, 1fr) var(--table-actions-column);
  }`;
  }).join("\n")}`;
  document.head.appendChild(style);
}

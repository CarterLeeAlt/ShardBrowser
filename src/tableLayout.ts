import layout from "./table-layout.json";

export function applyTableLayout() {
  const tables = [["browser", "t-cols"], ["proxy", "p-cols"]] as const;
  const styles = tables.map(([name, grid]) => {
    const table = layout[name];
    // The JSON lists the growing data columns in display order.
    const columns = [layout.dragColumn, layout.selectColumn, ...Object.values(table.columns)];
    const minimum = columns.reduce((sum, width) => sum + width + layout.columnGap, table.actions + table.paddingLeft + table.paddingRight);
    return { name, grid, table, columns, minimum };
  });
  const style = document.createElement("style");
  style.textContent = `:root {
    --layout-sidebar-width: ${layout.sidebarWidth}px;
    --main-min-padding: ${layout.minimumSidePadding}px;
    --scrollbar-width: ${layout.scrollbarWidth}px;
    --table-border: ${layout.tableBorder}px;
    --column-gap: ${layout.columnGap}px;
    --table-min-width: ${Math.max(...styles.map(({ minimum }) => minimum))}px;
  }
  ${styles.map(({ name, grid, table, columns, minimum }) => `.${name}-rows {
    --growing-columns: ${columns.length};
    --table-tracks-width: ${minimum}px;
    --table-padding-left: ${table.paddingLeft}px;
    --table-padding-right: ${table.paddingRight}px;
  }
  .${grid} {
    grid-template-columns: ${columns.map(width => `calc(${width}px + var(--column-extra))`).join(" ")} ${table.actions}px;
  }`).join("\n")}`;
  document.head.appendChild(style);
}

// Helper module exporting pure calculation functions.
export function add(a: number, b: number): number {
  return a + b;
}

export function formatResult(label: string, value: number): string {
  return `${label}: ${value}`;
}


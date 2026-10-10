export function validateObjectInput(
  raw: Record<string, unknown>,
  tool: string,
  allowedProperties: readonly string[],
): void {
  if (raw === null || typeof raw !== 'object' || Array.isArray(raw)) {
    throw new Error(`${tool}: input must be an object`);
  }
  const allowed = new Set(allowedProperties);
  for (const key of Object.keys(raw)) {
    if (!allowed.has(key)) throw new Error(`${tool}: unknown property ${key}`);
  }
}

export function optionalString(
  raw: Record<string, unknown>,
  key: string,
  tool: string,
): string | undefined {
  const value = raw[key];
  if (value === undefined) return undefined;
  if (typeof value !== 'string') throw new Error(`${tool}: ${key} must be a string`);
  return value;
}

export function optionalBoolean(
  raw: Record<string, unknown>,
  key: string,
  tool: string,
): boolean | undefined {
  const value = raw[key];
  if (value === undefined) return undefined;
  if (typeof value !== 'boolean') throw new Error(`${tool}: ${key} must be a boolean`);
  return value;
}

export function optionalNonNegativeInteger(
  raw: Record<string, unknown>,
  key: string,
  tool: string,
): number | undefined {
  const value = raw[key];
  if (value === undefined) return undefined;
  if (
    typeof value !== 'number' ||
    !Number.isSafeInteger(value) ||
    value < 0 ||
    value > 0xffff_ffff
  ) {
    throw new Error(`${tool}: ${key} must be a 32-bit unsigned integer`);
  }
  return value;
}

export function optionalPositiveInteger(
  raw: Record<string, unknown>,
  key: string,
  tool: string,
): number | undefined {
  const value = optionalNonNegativeInteger(raw, key, tool);
  if (value === 0) throw new Error(`${tool}: ${key} must be a positive safe integer`);
  return value;
}

export function optionalStringArray(
  raw: Record<string, unknown>,
  key: string,
  tool: string,
): string[] | undefined {
  const value = raw[key];
  if (value === undefined) return undefined;
  if (!Array.isArray(value) || value.some((item) => typeof item !== 'string')) {
    throw new Error(`${tool}: ${key} must be an array of strings`);
  }
  return value as string[];
}

export function requiredStringArray(
  raw: Record<string, unknown>,
  key: string,
  tool: string,
  minimum: number,
): string[] {
  const value = optionalStringArray(raw, key, tool);
  if (value === undefined || value.length < minimum) {
    throw new Error(`${tool}: ${key} must contain at least ${minimum} strings`);
  }
  return value;
}

export function optionalStringRecord(
  raw: Record<string, unknown>,
  key: string,
  tool: string,
): Record<string, string> | undefined {
  const value = raw[key];
  if (value === undefined) return undefined;
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`${tool}: ${key} must be an object with string values`);
  }
  for (const entry of Object.values(value)) {
    if (typeof entry !== 'string') {
      throw new Error(`${tool}: ${key} must be an object with string values`);
    }
  }
  return value as Record<string, string>;
}

export function optionalEnum<const T extends string>(
  raw: Record<string, unknown>,
  key: string,
  tool: string,
  values: readonly T[],
): T | undefined {
  const value = optionalString(raw, key, tool);
  if (value === undefined) return undefined;
  if (!values.includes(value as T)) {
    throw new Error(`${tool}: ${key} must be one of ${values.join(', ')}`);
  }
  return value as T;
}

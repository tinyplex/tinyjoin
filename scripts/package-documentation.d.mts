export function getPackageDocumentation(root: string): Promise<Record<string, string>>;
export function writePackageDocumentation(root: string, packageDirectory: string): Promise<void>;

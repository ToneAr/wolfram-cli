#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import {
	existsSync,
	mkdtempSync,
	readFileSync,
	renameSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(scriptDir, "..");
process.chdir(repoRoot);

function log(message: string): void {
	process.stderr.write(`${message}\n`);
}

function fail(message: string): never {
	log(`error: ${message}`);
	process.exit(1);
}

function which(command: string): string | undefined {
	const pathExt =
		process.platform === "win32"
			? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT").split(";")
			: [""];
	const dirs = (process.env.PATH ?? "").split(
		process.platform === "win32" ? ";" : ":",
	);
	for (const dir of dirs) {
		if (!dir) continue;
		for (const ext of pathExt) {
			const candidate = join(dir, command + ext);
			if (existsSync(candidate)) return candidate;
		}
	}
	return undefined;
}

function kernelBinaryName(): string {
	return process.platform === "win32" ? "WolframKernel.exe" : "WolframKernel";
}

function findKernel(): string | undefined {
	if (process.env.WOLFRAM_KERNEL) {
		return process.env.WOLFRAM_KERNEL;
	}

	const kernelName = kernelBinaryName();
	const wolframscript = which("wolframscript");
	if (wolframscript) {
		const result = spawnSync(wolframscript, ["-showkernels"], {
			encoding: "utf8",
		});
		if (result.status === 0 && result.stdout) {
			for (const rawLine of result.stdout.split("\n")) {
				const line = rawLine.trim();
				if (!line) continue;
				if (
					line.endsWith(`/${kernelName}`) ||
					line.endsWith(`\\${kernelName}`)
				) {
					if (existsSync(line)) return line;
				}
			}
		}
	}

	return which(kernelName);
}

function validateBuiltinSymbols(path: string): boolean {
	const contents = readFileSync(path, "utf8");
	let count = 0;
	let hasPlot = false;
	for (const line of contents.split("\n")) {
		if (!line) continue;
		const fields = line.split("\t");
		if (
			fields.length === 2 &&
			fields[0] !== "" &&
			/^[0-9]+$/.test(fields[1])
		) {
			count++;
			if (fields[0] === "Plot") hasPlot = true;
		}
	}
	return count > 1000 && hasPlot;
}

const kernel =
	findKernel() ??
	fail(
		"could not find WolframKernel. Set WOLFRAM_KERNEL to the kernel executable path.",
	);

const output = join(scriptDir, "builtin_symbols.tsv");
// Written alongside `output` (rather than under the system tmpdir) so the
// final rename is same-filesystem and can't fail with EXDEV.
const tmpDir = mkdtempSync(join(scriptDir, ".wolfish-builtin-symbols."));
const tmp = join(tmpDir, randomBytes(6).toString("hex"));

function cleanup(): void {
	rmSync(tmpDir, { recursive: true, force: true });
}

process.on("exit", cleanup);
for (const signal of ["SIGHUP", "SIGINT", "SIGTERM"] as const) {
	process.on(signal, () => process.exit(1));
}

log(`Generating ${output}`);
log(`Using kernel: ${kernel}`);

const query =
	'(Get["build_tools/wl/query_to_output_form.wl"])[(Get["build_tools/wl/builtin_symbols.wl"])[]]';
const run = spawnSync(kernel, ["-noprompt", "-run", query], {
	encoding: "utf8",
});
if (run.status !== 0) {
	fail("WolframKernel failed while generating builtin symbols");
}
writeFileSync(tmp, run.stdout);

if (!validateBuiltinSymbols(tmp)) {
	log(
		"Generated output did not look like a valid builtin symbol table. First lines:",
	);
	log(run.stdout.split("\n").slice(0, 20).join("\n"));
	fail(`refusing to replace ${output}`);
}

renameSync(tmp, output);

const lines = (readFileSync(output, "utf8").match(/\n/g) ?? []).length;
log(`Wrote ${output} (${lines} lines)`);

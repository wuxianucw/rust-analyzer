import fetch from "node-fetch";
var HttpsProxyAgent = require('https-proxy-agent');

import * as vscode from "vscode";
import * as stream from "stream";
import * as crypto from "crypto";
import * as fs from "fs";
import * as zlib from "zlib";
import * as util from "util";
import * as path from "path";
import { log, assert } from "./util";

const pipeline = util.promisify(stream.pipeline);

const GITHUB_API_ENDPOINT_URL = "https://api.github.com";
const OWNER = "rust-analyzer";
const REPO = "rust-analyzer";

export async function fetchRelease(
    releaseTag: string,
    githubToken: string | null | undefined,
    httpProxy: string | null | undefined,
): Promise<GithubRelease> {

    const apiEndpointPath = `/repos/${OWNER}/${REPO}/releases/tags/${releaseTag}`;

    const requestUrl = GITHUB_API_ENDPOINT_URL + apiEndpointPath;

    log.debug("Issuing request for released artifacts metadata to", requestUrl);

    const headers: Record<string, string> = { Accept: "application/vnd.github.v3+json" };
    if (githubToken != null) {
        headers.Authorization = "token " + githubToken;
    }

    const response = await (() => {
        if (httpProxy) {
            log.debug(`Fetching release metadata via proxy: ${httpProxy}`);
            return fetch(requestUrl, { headers: headers, agent: new HttpsProxyAgent(httpProxy) });
        }

        return fetch(requestUrl, { headers: headers });
    })();

    if (!response.ok) {
        log.error("Error fetching artifact release info", {
            requestUrl,
            releaseTag,
            response: {
                headers: response.headers,
                status: response.status,
                body: await response.text(),
            }
        });

        throw new Error(
            `Got response ${response.status} when trying to fetch ` +
            `release info for ${releaseTag} release`
        );
    }

    // We skip runtime type checks for simplicity (here we cast from `any` to `GithubRelease`)
    const release: GithubRelease = await response.json();
    return release;
}

// We omit declaration of tremendous amount of fields that we are not using here
export interface GithubRelease {
    name: string;
    id: number;
    // eslint-disable-next-line camelcase
    published_at: string;
    assets: Array<{
        name: string;
        // eslint-disable-next-line camelcase
        browser_download_url: vscode.Uri;
    }>;
}

interface DownloadOpts {
    progressTitle: string;
    url: vscode.Uri;
    dest: vscode.Uri;
    mode?: number;
    gunzip?: boolean;
    httpProxy?: string;
}

export async function download(opts: DownloadOpts) {
    // Put artifact into a temporary file (in the same dir for simplicity)
    // to prevent partially downloaded files when user kills vscode
    // This also avoids overwriting running executables
    const randomHex = crypto.randomBytes(5).toString("hex");
    const rawDest = path.parse(opts.dest.fsPath);
    const oldServerPath = vscode.Uri.joinPath(vscode.Uri.file(rawDest.dir), `${rawDest.name}-stale-${randomHex}${rawDest.ext}`);
    const tempFilePath = vscode.Uri.joinPath(vscode.Uri.file(rawDest.dir), `${rawDest.name}${randomHex}`);

    await vscode.window.withProgress(
        {
            location: vscode.ProgressLocation.Notification,
            cancellable: false,
            title: opts.progressTitle
        },
        async (progress, _cancellationToken) => {
            let lastPercentage = 0;
            await downloadFile(opts.url, tempFilePath, opts.mode, !!opts.gunzip, opts.httpProxy, (readBytes, totalBytes) => {
                const newPercentage = Math.round((readBytes / totalBytes) * 100);
                if (newPercentage !== lastPercentage) {
                    progress.report({
                        message: `${newPercentage.toFixed(0)}%`,
                        increment: newPercentage - lastPercentage
                    });

                    lastPercentage = newPercentage;
                }
            });
        }
    );

    // Try to rename a running server to avoid EPERM on Windows
    // NB: this can lead to issues if a running Code instance tries to restart the server.
    try {
        await vscode.workspace.fs.rename(opts.dest, oldServerPath, { overwrite: true });
        log.info(`Renamed old server binary ${opts.dest.fsPath} to ${oldServerPath.fsPath}`);
    } catch (err) {
        const fsErr = err as vscode.FileSystemError;
        // This is supposed to return `FileNotFound`, alas...
        if (!fsErr.code || fsErr.code !== "FileNotFound" && fsErr.code !== "EntryNotFound") {
            log.error(`Cannot rename existing server instance: ${err}`);
        }
    }
    try {
        await vscode.workspace.fs.rename(tempFilePath, opts.dest, { overwrite: true });
    } catch (err) {
        log.error(`Cannot update server binary: ${err}`);
    }

    // Now try to remove any stale server binaries
    const serverDir = vscode.Uri.file(rawDest.dir);
    try {
        const entries = await vscode.workspace.fs.readDirectory(serverDir);
        for (const [entry, _] of entries) {
            try {
                if (entry.includes(`${rawDest.name}-stale-`)) {
                    const uri = vscode.Uri.joinPath(serverDir, entry);
                    try {
                        await vscode.workspace.fs.delete(uri);
                        log.info(`Removed old server binary ${uri.fsPath}`);
                    } catch (err) {
                        log.error(`Unable to remove old server binary ${uri.fsPath}`);
                    }
                }
            } catch (err) {
                log.error(`Unable to parse ${entry}`);
            }
        }
    } catch (err) {
        log.error(`Unable to enumerate contents of ${serverDir.fsPath}`);
    }
}

async function downloadFile(
    url: vscode.Uri,
    destFilePath: vscode.Uri,
    mode: number | undefined,
    gunzip: boolean,
    httpProxy: string | null | undefined,
    onProgress: (readBytes: number, totalBytes: number) => void
): Promise<void> {
    const urlString = url.toString();

    const res = await (() => {
        if (httpProxy) {
            log.debug(`Downloading ${urlString} via proxy: ${httpProxy}`);
            return fetch(urlString, { agent: new HttpsProxyAgent(httpProxy) });
        }

        return fetch(urlString);
    })();

    if (!res.ok) {
        log.error("Error", res.status, "while downloading file from", urlString);
        log.error({ body: await res.text(), headers: res.headers });

        throw new Error(`Got response ${res.status} when trying to download a file.`);
    }

    const totalBytes = Number(res.headers.get('content-length'));
    assert(!Number.isNaN(totalBytes), "Sanity check of content-length protocol");

    log.debug("Downloading file of", totalBytes, "bytes size from", urlString, "to", destFilePath.fsPath);

    let readBytes = 0;
    res.body.on("data", (chunk: Buffer) => {
        readBytes += chunk.length;
        onProgress(readBytes, totalBytes);
    });

    const destFileStream = fs.createWriteStream(destFilePath.fsPath, { mode });
    const srcStream = gunzip ? res.body.pipe(zlib.createGunzip()) : res.body;

    await pipeline(srcStream, destFileStream);

    // Don't apply the workaround in fixed versions of nodejs, since the process
    // freezes on them, the process waits for no-longer emitted `close` event.
    // The fix was applied in commit 7eed9d6bcc in v13.11.0
    // See the nodejs changelog:
    // https://github.com/nodejs/node/blob/master/doc/changelogs/CHANGELOG_V13.md
    const [, major, minor] = /v(\d+)\.(\d+)\.(\d+)/.exec(process.version)!;
    if (+major > 13 || (+major === 13 && +minor >= 11)) return;

    await new Promise<void>(resolve => {
        destFileStream.on("close", resolve);
        destFileStream.destroy();
        // This workaround is awaiting to be removed when vscode moves to newer nodejs version:
        // https://github.com/rust-analyzer/rust-analyzer/issues/3167
    });
}

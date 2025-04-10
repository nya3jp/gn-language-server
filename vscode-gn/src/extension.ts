/**
 * Copyright 2025 Google LLC
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

import * as path from 'path';
import * as vscode from 'vscode';
import {Wasm, ProcessOptions} from '@vscode/wasm-wasi/v1';
import {
  createStdioOptions,
  createUriConverters,
  startServer,
} from '@vscode/wasm-wasi-lsp';
import {
  LanguageClient,
  LanguageClientOptions,
  MessageSignature,
  ResponseError,
  ServerOptions,
} from 'vscode-languageclient/node';

function ancestors(uri: vscode.Uri): vscode.Uri[] {
  const ancestors = [];
  let current = uri;
  for (;;) {
    ancestors.push(current);
    const dir = path.dirname(current.path);
    if (dir === current.path) {
      break;
    }
    current = current.with({path: dir});
  }
  return ancestors;
}

async function statNoThrow(
  uri: vscode.Uri
): Promise<vscode.FileStat | undefined> {
  try {
    return await vscode.workspace.fs.stat(uri);
  } catch {
    return undefined;
  }
}

async function isInGnWorkspace(uri: vscode.Uri): Promise<boolean> {
  for (const dirUri of ancestors(uri).slice(1)) {
    for (const name of ['.gn', 'BUILD.gn']) {
      const candidateUri = dirUri.with({path: path.join(dirUri.path, name)});
      if (await statNoThrow(candidateUri)) {
        return true;
      }
    }
  }
  return false;
}

async function updateActiveEditorContext(): Promise<void> {
  const uri = vscode.window.activeTextEditor?.document?.uri;
  const inGnWorkspace = uri ? await isInGnWorkspace(uri) : false;
  vscode.commands.executeCommand(
    'setContext',
    'gn.inGnWorkspace',
    inGnWorkspace
  );
}

async function openBuildFile(): Promise<void> {
  const startUri = vscode.window.activeTextEditor?.document?.uri;
  if (!startUri) {
    void vscode.window.showErrorMessage('No open editor.');
    return;
  }

  const isGnFile =
    startUri.path.endsWith('.gn') || startUri.path.endsWith('.gni');

  if (isGnFile) {
    const dotGnUri = startUri.with({
      path: path.join(path.dirname(startUri.path), '.gn'),
    });
    if (await statNoThrow(dotGnUri)) {
      void vscode.window.showInformationMessage(
        'This file is in the top-level directory.'
      );
      return;
    }
  }

  for (const dirUri of ancestors(startUri).slice(isGnFile ? 2 : 1)) {
    const buildUri = dirUri.with({path: path.join(dirUri.path, 'BUILD.gn')});
    if (await statNoThrow(buildUri)) {
      vscode.window.showTextDocument(buildUri);
      return;
    }
    if (await statNoThrow(dirUri.with({path: path.join(dirUri.path, '.gn')}))) {
      break;
    }
  }

  void vscode.window.showErrorMessage(
    'BUILD.gn not found in the ancestor directories.'
  );
}

class GnLanguageClient extends LanguageClient {
  private constructor(id: string, name: string, serverOptions: ServerOptions, clientOptions: LanguageClientOptions) {
    super(id, name, serverOptions, clientOptions);
  }

  static async create(context: vscode.ExtensionContext, output: vscode.OutputChannel): Promise<GnLanguageClient> {
    const wasm = await Wasm.load();

    const clientOptions: LanguageClientOptions = {
      documentSelector: [{pattern: '**/*.gn'}, {pattern: '**/*.gni'}],
      synchronize: {
        configurationSection: 'gn',
        fileEvents: [
          vscode.workspace.createFileSystemWatcher('**/*.gn'),
          vscode.workspace.createFileSystemWatcher('**/*.gni'),
        ],
      },
      outputChannel: output,
      uriConverters: createUriConverters(),
    };

    const serverOptions: ServerOptions = async () => {
      const options: ProcessOptions = {
        stdio: createStdioOptions(),
        mountPoints: [{kind: 'workspaceFolder'}],
      };
      const filename = vscode.Uri.joinPath(
        context.extensionUri,
        'dist',
        'gn-language-server.wasm'
      );
      const bits = await vscode.workspace.fs.readFile(filename);
      const module = await WebAssembly.compile(bits);
      const process = await wasm.createProcess(
        'gn-language-server',
        module,
        {initial: 16 * 16, maximum: 16 * 1024, shared: true},
        options
      );

      const decoder = new TextDecoder('utf-8');
      process.stderr!.onData(data => {
        output.append(decoder.decode(data));
      });

      return startServer(process);
    };

    return new GnLanguageClient('gn', 'GN', serverOptions, clientOptions);
  }

  handleFailedRequest<T>(
    type: MessageSignature,
    token: vscode.CancellationToken | undefined,
    error: unknown,
    defaultValue: T,
    showNotification?: boolean
  ): T {
    if (error instanceof ResponseError && error.code === 1) {
      this.error(`${type.method}: ${error.message}`, true);
      throw error;
    }
    return super.handleFailedRequest(
      type,
      token,
      error,
      defaultValue,
      showNotification
    );
  }
}

async function startLanguageServer(
  context: vscode.ExtensionContext,
  output: vscode.OutputChannel
): Promise<void> {
  const client = await GnLanguageClient.create(context, output);
  context.subscriptions.push(client);
  await client.start();
}

export async function activate(
  context: vscode.ExtensionContext
): Promise<void> {
  const output = vscode.window.createOutputChannel('GN');
  context.subscriptions.push(output);

  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor(updateActiveEditorContext)
  );
  await updateActiveEditorContext();

  context.subscriptions.push(
    vscode.commands.registerCommand('gn.openBuildFile', openBuildFile)
  );

  await startLanguageServer(context, output);
}

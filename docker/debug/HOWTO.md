# Remote Debugging tuliprox in VSCode with Docker

This guide explains how to set up remote debugging for the tuliprox project in VSCode, using either **GDB** or **LLDB** inside a Docker container.

## Prerequisites

- VSCode with the following extensions:
  - **C/C++** from Microsoft (for GDB support): [Marketplace Link](https://marketplace.visualstudio.com/items?itemName=ms-vscode.cpptools)
  - **CodeLLDB** (for LLDB support): [Marketplace Link](https://marketplace.visualstudio.com/items?itemName=vadimcn.vscode-lldb)
  - **Docker**: [Marketplace Link](https://marketplace.visualstudio.com/items?itemName=ms-azuretools.vscode-docker)
- Docker and Docker Compose installed.
- Rust toolchain (for local symbol resolution).

## Setup

1. Copy the debug configuration files to your project's root directory:

    ```bash
    # Copy VSCode tasks and launch configurations
    cp -r docker/debug/.vscode .

    # Copy the Docker Compose override file for debugging
    cp docker/debug/docker-compose.debug-override.yml .
    ```

## Configuration

### 1. Choose Your Debugger (`docker-compose.debug-override.yml`)

The `docker-compose.debug-override.yml` file is the central point for configuring your debug session. You must choose which debugger to use by setting
the `DEBUG_SERVER` environment variable.

- Open `docker-compose.debug-override.yml`.
- Under the `environment` section for the `tuliprox` service, set `DEBUG_SERVER` to either `gdb` or `lldb`.

**Example for using GDB:**

```yaml
services:
  tuliprox:
    # ... other settings
    environment:
      - DEBUG_SERVER=gdb
      - RUST_TARGET=x86_64-unknown-linux-musl
```

**Example for using LLDB:**

```yaml
services:
  tuliprox:
    # ... other settings
    environment:
      - DEBUG_SERVER=lldb
      - RUST_TARGET=x86_64-unknown-linux-musl
```

This file also:

- Exposes the debug port (default: `10586`).
- Sets up privileged mode, which is required for some debuggers.
- Mounts cargo cache volumes for faster subsequent builds.

### 2. VSCode Launch Configurations (`.vscode/launch.json`)

This file contains pre-configured launch profiles for LLDB (preferred debugger in VSCode).
You don't need to edit this file, just select the correct profile in VSCode.

### 3. VSCode Tasks (`.vscode/tasks.json`)

- **docker-compose-up-debug**: Builds and starts the container with your debug configuration.
- **docker-compose-down**: Stops and removes the container.

## Debugging Workflow

1. **Configure Debugger**: Open `docker-compose.debug-override.yml` and set the `DEBUG_SERVER` variable to either `gdb` or `lldb`.
2. **Open Project**: Open the project in VSCode.
3. **Set Breakpoints**: Place breakpoints in your Rust code.
4. **Start Debugging**:
    - Go to the "Run and Debug" panel (Ctrl+Shift+D).
    - Select the launch configuration that matches your choice in step 1 (e.g., "Docker: GDB Remote Attach").
    - Press **F5** to start the `preLaunchTask`, which will build and run your Docker container, and then attach the debugger.

## Troubleshooting

### 1. Debugger Fails to Connect or Disconnects Immediately

- **Check `DEBUG_SERVER` variable**: Ensure the `DEBUG_SERVER` variable in `docker-compose.debug-override.yml` matches the launch configuration you
  selected in VSCode.
- **Check Ports**: Verify that the debug port (default `10586`) is not being used by another application on your host machine.
- **Check Container Logs**: Look at the Docker container logs. You should see output from `debug-entrypoint.sh` indicating that the debug server
  (GDB or LLDB) has started and is listening.

### 2. Breakpoints Are Not Being Hit

- **Verify Build Target**: Ensure the `RUST_TARGET` in your `docker-compose.debug-override.yml` matches the target you are building for.
- **Check Source Mapping**:
  - For GDB (`cppdbg`), verify the `sourceFileMap` in `.vscode/launch.json`. The remote path (`/usr/src/tuliprox`) should map to your local project
    directory (`${workspaceFolder}`).
  - For LLDB, verify the `sourceMap` setting.
- **Debug Build**: Make sure you are running a debug build, not a release build, as release builds may optimize out debug symbols. The provided
  scripts default to a debug build.

### 4. Performance Issues

- The debug build is significantly slower and uses more memory than a release build. This is normal.
- Consider allocating more resources (CPU/RAM) to Docker if your application is complex.

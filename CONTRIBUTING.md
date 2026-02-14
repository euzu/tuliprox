# Contributing

Thank you for your interest in contributing to Tuliprox! We welcome contributions of all kinds, including bug fixes, new features, documentation improvements, and more. To ensure a smooth contribution process, please follow the guidelines outlined below.

## How to Contribute

### Prerequisites

To contribute you will need Rust, cargo, cross, trunk, wasm-bindgen, and cargo-set-version installed. You can install them using the following command:

```bash
make install-tools
```

To force a reinstall of tooling or updating to the latest version, you can run:

```bash
make -B install-tools
```

### Steps to Contribute

1. **Fork the Repository**: Start by forking the Tuliprox repository to your GitHub account.
2. **Clone Your Fork**: Clone the forked repository to your local machine.

   ```bash
   git clone git@github.com:your-username/tuliprox.git
   cd tuliprox
   ```

3. **Create a Branch**: Create a new branch for your contribution.

   ```bash
   git checkout -b feature/your-feature-name
   ```

4. **Make Your Changes**: Implement your changes in the codebase. Please ensure that your code follows the existing style and conventions.
5. **Test Your Changes**: Format, lint, and run tests to ensure that your changes are clean and tidy and do not break existing functionality.

    ```bash
    make fmt lint test
    ```

6. **Commit Your Changes**: Commit your changes with a descriptive message.

   ```bash
   git add .
   git commit -m "Add your descriptive message here"
   ```

7. **Push Your Changes**: Push your changes to your forked repository.

   ```bash
   git push origin feature/your-feature-name
   ```

8. **Create a Pull Request**: Open a pull request on the original Tuliprox repository, describing your changes and their purpose.
9. **Address Feedback**: Be responsive to any feedback or requests for changes from the maintainers.

// Package build builds the installers of a local checkout for each system:
// Linux in the repository's Docker build on this computer, Windows and macOS
// on build machines over SSH. The working tree is what gets built, with
// changes that are not committed.
package build

import (
	"archive/tar"
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"slices"
	"strings"
	"sync"

	"github.com/jetkvm/os-services/tools/e2e/ssh"
)

// Config is the "build" entry of a lab file for one system.
type Config struct {
	// Docker builds on this computer with the repository's Dockerfile
	// (Linux only), as CI does.
	Docker bool `json:"docker,omitempty"`
	// SSH is the login of a build machine, and Dir its folder for the
	// tree; the folder keeps its target/ between builds.
	SSH string `json:"ssh,omitempty"`
	Dir string `json:"dir,omitempty"`
	// SigningIdentity signs a macOS build with a keychain identity, so that
	// the test Mac keeps its privacy permissions from one build to the next.
	SigningIdentity string `json:"signing_identity,omitempty"`
	// Keychain holds that identity and is unlocked with KeychainPassword
	// before the build: an SSH session cannot use a keychain that another
	// session unlocked. Use a keychain only for builds, not the login one.
	Keychain         string `json:"keychain,omitempty"`
	KeychainPassword string `json:"keychain_password,omitempty"`
}

// installerSuffix ends the file name of each system's installer in dist/.
var installerSuffix = map[string]string{
	"linux":   ".AppImage",
	"windows": "-setup.exe",
	"macos":   ".app.tar.gz",
}

// All builds root (a checkout) for each system in configs at the same time,
// into out/<system>/, and returns each system's installer. log gets one
// line per finished build; each build's output is in out/<system>.log.
func All(ctx context.Context, root, out string, configs map[string]Config, log func(string, ...any)) (map[string]string, error) {
	var mu sync.Mutex
	var wg sync.WaitGroup
	installers := map[string]string{}
	var errs []string
	for system, config := range configs {
		wg.Add(1)
		go func() {
			defer wg.Done()
			installer, err := one(ctx, root, filepath.Join(out, system), system, config)
			mu.Lock()
			defer mu.Unlock()
			if err != nil {
				errs = append(errs, fmt.Sprintf("%s: %v (log: %s.log)", system, err, filepath.Join(out, system)))
				return
			}
			installers[system] = installer
			log("built %s: %s", system, filepath.Base(installer))
		}()
	}
	wg.Wait()
	if len(errs) > 0 {
		return nil, fmt.Errorf("build failed: %s", strings.Join(errs, "; "))
	}
	return installers, nil
}

func one(ctx context.Context, root, out, system string, config Config) (string, error) {
	if _, ok := installerSuffix[system]; !ok {
		return "", fmt.Errorf("no build for system %q", system)
	}
	if err := os.RemoveAll(out); err != nil {
		return "", err
	}
	if err := os.MkdirAll(out, 0o755); err != nil {
		return "", err
	}
	logFile, err := os.Create(out + ".log")
	if err != nil {
		return "", err
	}
	defer logFile.Close()
	switch {
	case config.Docker && system == "linux":
		err = docker(ctx, root, out, logFile)
	case config.SSH != "" && config.Dir != "":
		err = remote(ctx, root, out, system, config, logFile)
	default:
		err = fmt.Errorf("the build entry needs docker (Linux) or ssh and dir")
	}
	if err != nil {
		return "", err
	}
	return pick(out, system)
}

// docker runs the repository's Docker build, the one CI's Linux job runs.
func docker(ctx context.Context, root, out string, log io.Writer) error {
	command := exec.CommandContext(ctx, "docker", "build", "--target", "dist", "--output", "type=local,dest="+out, root)
	command.Stdout, command.Stderr = log, log
	return command.Run()
}

// remote sends the working tree to the build machine, builds it there as
// CI does and fetches dist/ back as a tar stream.
func remote(ctx context.Context, root, out, system string, config Config, log io.Writer) error {
	if err := checkDir(config.Dir); err != nil {
		return err
	}
	tree, err := archive(ctx, root)
	if err != nil {
		return err
	}
	dir := remotePath(config.Dir)
	if err := runWithInput(ctx, config.SSH, receiveScript(dir), bytes.NewReader(tree), log); err != nil {
		return fmt.Errorf("send the tree: %w", err)
	}
	if err := runWithInput(ctx, config.SSH, "set -e; cd "+dir+"; "+environment(system, config)+"cargo xtask setup && cargo xtask package"+packageFlags(system), nil, log); err != nil {
		return fmt.Errorf("build: %w", err)
	}
	var dist bytes.Buffer
	if err := runWithOutput(ctx, config.SSH, "cd "+dir+"/dist && tar -cf - .", &dist, log); err != nil {
		return fmt.Errorf("fetch dist/: %w", err)
	}
	untar := exec.CommandContext(ctx, "tar", "-xf", "-", "-C", out)
	untar.Stdin, untar.Stdout, untar.Stderr = &dist, log, log
	return untar.Run()
}

// marker is the file that makes a folder of the build machine a build
// folder: receiveScript writes it, and deletes files only where it is.
const marker = ".os-services-e2e-build"

// checkDir refuses a build folder that is a home folder or a root, or
// that has ".." in its path. receiveScript deletes the contents of the
// folder.
func checkDir(dir string) error {
	rest := strings.ReplaceAll(dir, `\`, "/")
	if rest == "~" || strings.HasPrefix(rest, "~/") {
		rest = rest[1:]
	} else if len(rest) >= 2 && rest[1] == ':' {
		rest = rest[2:] // a Windows drive
	}
	if slices.Contains(strings.Split(rest, "/"), "..") {
		return fmt.Errorf(`the build folder %q has ".." in its path`, dir)
	}
	if rest = path.Clean(rest); rest == "." || rest == "/" {
		return fmt.Errorf("the build folder %q is a home folder or a root; give a folder only for builds", dir)
	}
	return nil
}

// receiveScript replaces the tree in dir (one shell word) with the tar
// stream on stdin, and keeps target/ for incremental builds. It deletes
// files only in a build folder: a folder with the marker, which it writes
// into a folder that is empty. It stops before it deletes a file if dir
// is the home folder or contains the home folder, or if dir has files and
// no marker.
func receiveScript(dir string) string {
	return fmt.Sprintf(`set -e; mkdir -p %[1]s; cd %[1]s; here=$(pwd -P); home=$(cd "$HOME" && pwd -P); `+
		`case "$home/" in "${here%%/}"/*) echo "the build folder $here is the home folder or contains it; give a folder only for builds" >&2; exit 1;; esac; `+
		`if [ ! -e %[2]s ] && [ -n "$(ls -A)" ]; then echo "the build folder $here has files, and no %[2]s file marks it as a build folder; give a new or empty folder" >&2; exit 1; fi; `+
		`: >%[2]s; find . -mindepth 1 -maxdepth 1 ! -name target ! -name %[2]s -exec rm -rf {} +; tar -xf -`, dir, marker)
}

// environment is what CI's job sets for the system.
func environment(system string, config Config) string {
	switch system {
	case "windows":
		return `export PATH="/c/Users/$USERNAME/.cargo/bin:/c/Program Files/PowerShell/7:$PATH"; ` +
			`export VCPKG_ROOT='C:\vcpkg' VCPKG_BINARY_SOURCES='clear;files,C:\vcpkg-cache,readwrite'; `
	case "macos":
		env := `export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"; ` +
			`export VCPKG_BINARY_SOURCES="clear;files,$PWD/target/vcpkg-cache,readwrite"; `
		if config.Keychain != "" {
			env += "security unlock-keychain -p " + ssh.Quote(config.KeychainPassword) + " " + remotePath(config.Keychain) + "; "
		}
		if config.SigningIdentity != "" {
			env += "export APPLE_SIGNING_IDENTITY=" + ssh.Quote(config.SigningIdentity) + "; "
		}
		return env
	}
	return ""
}

// remotePath is a path of the build machine as one shell word. A leading
// "~/" stays outside the quotes, so the shell still expands it.
func remotePath(path string) string {
	if rest, ok := strings.CutPrefix(path, "~/"); ok {
		return `"$HOME"/` + ssh.Quote(rest)
	}
	return ssh.Quote(path)
}

func packageFlags(system string) string {
	if system == "macos" {
		return " --skip-notarization"
	}
	return ""
}

// archive is the working tree as a tar stream: the tracked files and the
// files that are not ignored, as they are on disk. A tracked file that was
// deleted is left out, and so is a submodule's folder.
func archive(ctx context.Context, root string) ([]byte, error) {
	list := exec.CommandContext(ctx, "git", "-C", root, "ls-files", "-z", "--cached", "--others", "--exclude-standard")
	files, err := list.Output()
	if err != nil {
		return nil, fmt.Errorf("git ls-files: %w", err)
	}
	var tree bytes.Buffer
	writer := tar.NewWriter(&tree)
	for name := range strings.SplitSeq(strings.TrimSuffix(string(files), "\x00"), "\x00") {
		if err := addFile(writer, root, name); err != nil {
			return nil, fmt.Errorf("tar the tree: %w", err)
		}
	}
	if err := writer.Close(); err != nil {
		return nil, fmt.Errorf("tar the tree: %w", err)
	}
	return tree.Bytes(), nil
}

// addFile writes the file or symbolic link name of root to writer.
func addFile(writer *tar.Writer, root, name string) error {
	path := filepath.Join(root, filepath.FromSlash(name))
	info, err := os.Lstat(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	var link string
	switch {
	case info.Mode().IsRegular():
	case info.Mode()&fs.ModeSymlink != 0:
		if link, err = os.Readlink(path); err != nil {
			return err
		}
	default:
		return nil
	}
	header, err := tar.FileInfoHeader(info, link)
	if err != nil {
		return err
	}
	// This computer's owners mean nothing on the build machine.
	header.Name, header.Uid, header.Gid, header.Uname, header.Gname = name, 0, 0, "", ""
	if err := writer.WriteHeader(header); err != nil {
		return err
	}
	if link != "" {
		return nil
	}
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	_, err = io.Copy(writer, file)
	return err
}

// pick finds the system's installer in out.
func pick(out, system string) (string, error) {
	entries, err := os.ReadDir(out)
	if err != nil {
		return "", err
	}
	for _, entry := range entries {
		if strings.HasSuffix(entry.Name(), installerSuffix[system]) {
			// Tauri names the Windows installer after the product, with
			// spaces; the install commands take plain file names, as the
			// release files have.
			file := filepath.Join(out, strings.ReplaceAll(entry.Name(), " ", "-"))
			return file, os.Rename(filepath.Join(out, entry.Name()), file)
		}
	}
	return "", fmt.Errorf("no %s installer in %s", system, out)
}

func runWithInput(ctx context.Context, login, script string, input io.Reader, log io.Writer) error {
	return ssh.Stream(ctx, login, script, input, log, log)
}

func runWithOutput(ctx context.Context, login, script string, output, log io.Writer) error {
	return ssh.Stream(ctx, login, script, nil, output, log)
}

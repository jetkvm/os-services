// Package ssh runs POSIX shell scripts on other computers.
package ssh

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
)

// A host key that ssh has not seen is trusted at its first use; a changed
// one still stops the connection.
var options = []string{"-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=accept-new"}

// Run runs script with sh on login (user@host or user@host:port) and
// returns its output. The login shell of the computer can be another shell,
// so the script goes to sh -c.
func Run(ctx context.Context, login, script string) (string, error) {
	var stdout, stderr bytes.Buffer
	command, err := sshCommand(ctx, login, script)
	if err != nil {
		return "", err
	}
	command.Stdout, command.Stderr = &stdout, &stderr
	if err := command.Run(); err != nil {
		message := strings.TrimSpace(stderr.String())
		if message == "" {
			message = err.Error()
		}
		return stdout.String(), fmt.Errorf("%s: %s", login, message)
	}
	return stdout.String(), nil
}

// Stream runs script like Run, with stdin from input (or nothing) and its
// output written to stdout and stderr as it comes, for long commands and
// for data such as tar streams.
func Stream(ctx context.Context, login, script string, input io.Reader, stdout, stderr io.Writer) error {
	command, err := sshCommand(ctx, login, script)
	if err != nil {
		return err
	}
	command.Stdin, command.Stdout, command.Stderr = input, stdout, stderr
	if err := command.Run(); err != nil {
		return fmt.Errorf("%s: %w", login, err)
	}
	return nil
}

func sshCommand(ctx context.Context, login, script string) (*exec.Cmd, error) {
	var target []string
	if destination, port, ok := splitPort(login); ok {
		target = []string{"-p", port, destination}
	} else {
		target = []string{login}
	}
	keys, err := identities(ctx, target)
	if err != nil {
		return nil, err
	}
	args := append(append(slices.Clone(options), keys...), target...)
	return exec.CommandContext(ctx, "ssh", append(args, "sh -c "+Quote(script))...), nil
}

// The lab key is the tool's own SSH key, in
// $XDG_STATE_HOME/os-services-e2e/lab-key (default ~/.local/state), a
// folder of its own that only the user can open, as sshd requires of a
// folder with authorized keys. Test computers
// that the lab builds, such as the containers in distros/, trust only this
// key, so no personal key goes into them. A computer of your own keeps the
// keys it trusts: ssh offers each key, and the computer accepts the one in
// its authorized_keys.
var labKey = sync.OnceValues(func() (string, error) {
	state := os.Getenv("XDG_STATE_HOME")
	if state == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		state = filepath.Join(home, ".local", "state")
	}
	key := filepath.Join(state, "os-services-e2e", "lab-key", "id_ed25519")
	if _, err := os.Stat(key); err == nil {
		return key, nil
	}
	if err := os.MkdirAll(filepath.Dir(key), 0o700); err != nil {
		return "", err
	}
	if out, err := exec.Command("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "os-services-e2e", "-f", key).CombinedOutput(); err != nil {
		return "", fmt.Errorf("create the lab key: %v: %s", err, out)
	}
	return key, nil
})

// identities are the -i options for ssh to target: the key files that the
// ssh configuration gives target, in its order, then the lab key. ssh
// offers -i files before the configuration's IdentityFile entries, so the
// configured files are named first to keep them first, and ssh ignores a
// file that is named twice. Without IdentityFile entries, the
// configuration gives ssh's default files. Keys in the agent still come
// first.
func identities(ctx context.Context, target []string) ([]string, error) {
	key, err := labKey()
	if err != nil {
		return nil, err
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return nil, err
	}
	config, err := exec.CommandContext(ctx, "ssh", append([]string{"-G"}, target...)...).Output()
	if err != nil {
		return nil, fmt.Errorf("ssh -G %s: %w", strings.Join(target, " "), err)
	}
	return identityOptions(string(config), home, key), nil
}

// identityOptions reads the identityfile lines of `ssh -G` output and names
// the files that exist, then the lab key.
func identityOptions(config, home, labKey string) []string {
	var options []string
	for line := range strings.Lines(config) {
		file, ok := strings.CutPrefix(strings.TrimSpace(line), "identityfile ")
		if !ok {
			continue
		}
		if rest, ok := strings.CutPrefix(file, "~/"); ok {
			file = filepath.Join(home, rest)
		}
		if fileExists(file) {
			options = append(options, "-i", file)
		}
	}
	return append(options, "-i", labKey)
}

func fileExists(file string) bool {
	info, err := os.Stat(file)
	return err == nil && !info.IsDir()
}

// LabPublicKey is the lab key's public half, for the authorized_keys of a
// test computer.
func LabPublicKey() (string, error) {
	key, err := labKey()
	return key + ".pub", err
}

// Quote makes s one word for sh.
func Quote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

// Address is the host part of a login, without a port.
func Address(login string) string {
	if destination, _, ok := splitPort(login); ok {
		login = destination
	}
	return login[strings.LastIndex(login, "@")+1:]
}

// splitPort splits "user@host:port" into "user@host" and the port.
func splitPort(login string) (string, string, bool) {
	i := strings.LastIndex(login, ":")
	if i < strings.LastIndex(login, "@") || i < 0 {
		return login, "", false
	}
	port := login[i+1:]
	if _, err := strconv.ParseUint(port, 10, 16); err != nil {
		return login, "", false
	}
	return login[:i], port, true
}

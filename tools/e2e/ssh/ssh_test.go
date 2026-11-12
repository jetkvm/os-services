package ssh

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestLoginsWithAndWithoutPort(t *testing.T) {
	for login, want := range map[string][3]string{
		"tester@127.0.0.1:2201": {"tester@127.0.0.1", "2201", "127.0.0.1"},
		"tester@192.0.2.20":     {"tester@192.0.2.20", "", "192.0.2.20"},
		"tester@host:notaport":  {"tester@host:notaport", "", "host:notaport"},
		"host:22":               {"host", "22", "host"},
	} {
		destination, port, _ := splitPort(login)
		if got := [3]string{destination, port, Address(login)}; got != want {
			t.Errorf("%s: got %q, want %q", login, got, want)
		}
	}
}

func TestIdentitiesKeepTheConfiguredOrderAndAddTheLabKey(t *testing.T) {
	home := t.TempDir()
	if err := os.Mkdir(filepath.Join(home, ".ssh"), 0o700); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{".ssh/id_ed25519", ".ssh/id_rsa", ".ssh/work"} {
		if err := os.WriteFile(filepath.Join(home, name), nil, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	// A folder with a key's name is not a key.
	if err := os.Mkdir(filepath.Join(home, ".ssh", "id_ecdsa"), 0o700); err != nil {
		t.Fatal(err)
	}
	// `ssh -G` output for a host with an IdentityFile entry, which comes
	// before the default files that a Host * entry names.
	config := "user tester\nidentityfile ~/.ssh/work\nidentityfile ~/.ssh/id_rsa\n" +
		"identityfile ~/.ssh/id_ecdsa\nidentityfile ~/.ssh/id_ed25519\nidentityfile ~/.ssh/id_xmss\nport 22\n"
	got := strings.Join(identityOptions(config, home, "/state/lab"), " ")
	want := strings.Join([]string{
		"-i", filepath.Join(home, ".ssh/work"),
		"-i", filepath.Join(home, ".ssh/id_rsa"),
		"-i", filepath.Join(home, ".ssh/id_ed25519"),
		"-i", "/state/lab",
	}, " ")
	if got != want {
		t.Errorf("got %s, want %s", got, want)
	}
	if got := strings.Join(identityOptions("user tester\n", home, "/state/lab"), " "); got != "-i /state/lab" {
		t.Errorf("without identity files: got %s", got)
	}
}

package main

import (
	"strings"
	"testing"

	"github.com/jetkvm/os-services/tools/e2e/check"
)

func TestSelectChecksAlwaysStartsWithTheSession(t *testing.T) {
	selected, err := selectChecks("video,files")
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, c := range selected {
		names = append(names, c.Name)
	}
	if got := strings.Join(names, ","); got != "session,files,video" {
		t.Errorf("got %s, want session,files,video: the session first, then the order of check.All", got)
	}
	if _, err := selectChecks("vidoe"); err == nil {
		t.Error("an unknown check is refused")
	}
}

func TestAComputerLeavesOutOnlyChecksThatExistAndNeverTheSession(t *testing.T) {
	run, err := withoutSkipped(check.All, []string{"clipboard-secret"})
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, c := range run {
		names = append(names, c.Name)
	}
	if got := strings.Join(names, ","); got != "session,clipboard,files,video,forget,reconnect" {
		t.Errorf("got %s, want every check but clipboard-secret", got)
	}
	for _, name := range []string{"session", "clipbord-secret"} {
		if _, err := withoutSkipped(check.All, []string{name}); err == nil {
			t.Errorf("skip %q is refused", name)
		}
	}
}

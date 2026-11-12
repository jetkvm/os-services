package kvm

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/cookiejar"
	"net/url"
	"strings"
)

// web is the KVM's HTTP API with the login cookie.
type web struct {
	base   string
	client *http.Client
}

func newWeb(rawURL string) (*web, error) {
	parsed, err := url.Parse(rawURL)
	if err != nil || (parsed.Scheme != "http" && parsed.Scheme != "https") || parsed.Host == "" {
		return nil, fmt.Errorf("KVM URL %q is not an http or https address", rawURL)
	}
	jar, err := cookiejar.New(nil)
	if err != nil {
		return nil, err
	}
	return &web{base: parsed.Scheme + "://" + parsed.Host, client: &http.Client{
		Jar: jar,
		// A redirect to the login page must not look like an answer.
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}}, nil
}

// login gets the login cookie for password.
func (w *web) login(ctx context.Context, password string) error {
	if err := w.call(ctx, http.MethodPost, "/auth/login-local", map[string]string{"password": password}, nil); err != nil {
		return fmt.Errorf("log in: %w", err)
	}
	return nil
}

// statusError is an answer whose status is not 200 OK.
type statusError struct {
	code int
	text string
}

func (e *statusError) Error() string { return e.text }

// call sends body as JSON (when not nil) and decodes the answer into result
// (when not nil).
func (w *web) call(ctx context.Context, method, path string, body, result any) error {
	answer, err := w.raw(ctx, method, path, body)
	if err != nil || result == nil {
		return err
	}
	return json.Unmarshal(answer, result)
}

// raw sends body as JSON (when not nil) and returns the answer's bytes.
func (w *web) raw(ctx context.Context, method, path string, body any) ([]byte, error) {
	var reader io.Reader
	if body != nil {
		payload, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		reader = bytes.NewReader(payload)
	}
	request, err := http.NewRequestWithContext(ctx, method, w.base+path, reader)
	if err != nil {
		return nil, err
	}
	if body != nil {
		request.Header.Set("Content-Type", "application/json")
	}
	// A JetKVM answers a GET of a path it does not have with its web page,
	// except when the request does not accept HTML.
	request.Header.Set("Accept", "application/json")
	response, err := w.client.Do(request)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	answer, err := io.ReadAll(io.LimitReader(response.Body, 16<<20))
	if err != nil {
		return nil, err
	}
	if response.StatusCode != http.StatusOK {
		return nil, &statusError{code: response.StatusCode,
			text: fmt.Sprintf("%s %s: %s: %s", method, path, response.Status, strings.TrimSpace(string(answer)))}
	}
	return answer, nil
}

// unauthorized reports whether err is a 401 Unauthorized answer.
func unauthorized(err error) bool {
	var status *statusError
	return errors.As(err, &status) && status.code == http.StatusUnauthorized
}

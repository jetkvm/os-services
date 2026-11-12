package kvm

import (
	"context"
	"fmt"
	"time"
)

// Until reads the KVM's state until ok accepts it, for at most limit.
func (k *KVM) Until(ctx context.Context, limit time.Duration, what string, ok func(State) bool) (State, error) {
	ctx, cancel := context.WithTimeout(ctx, limit)
	defer cancel()
	for {
		state, err := k.State(ctx)
		if err == nil && ok(state) {
			return state, nil
		}
		select {
		case <-ctx.Done():
			return state, fmt.Errorf("%s: not within %s (last state: %+v, error: %v)", what, limit, state, err)
		case <-time.After(500 * time.Millisecond):
		}
	}
}

// GuestVideo turns video off and on and waits until the KVM shows the
// service's. The KVM allows 3 video switches in 2 minutes, across sessions,
// and a switch leaves that count only after 10 seconds of video. The targets
// before this one used switches of the same KVM; turning video off and on
// clears the count, as the KVM tells a user to do. The KVM shows the
// service's video until the service confirms that it is off, at most 15
// seconds, so video goes on again only after the KVM shows its own capture.
// A resolution such as "1920x1080" goes to the KVM while video is off, so
// that the switch presents it from the start, and the wait ends only when
// the KVM shows video of that size.
func (k *KVM) GuestVideo(ctx context.Context, resolution string) (State, error) {
	off := map[string]any{"video_enabled": false}
	if resolution != "" {
		off["resolution"] = resolution
	}
	if err := k.SetSettings(ctx, off); err != nil {
		return State{}, err
	}
	if _, err := k.Until(ctx, 30*time.Second, "the KVM's own video", func(s State) bool {
		return s.Source == "physical" && !s.OSServices.Video
	}); err != nil {
		return State{}, err
	}
	if err := k.SetSettings(ctx, map[string]any{"video_enabled": true}); err != nil {
		return State{}, err
	}
	state, err := k.Until(ctx, 60*time.Second, "service video", func(s State) bool {
		size := fmt.Sprintf("%dx%d", s.Width, s.Height)
		return s.OSServices.VideoError != "" ||
			(s.Source == "guest" && s.OSServices.Video && s.Ready && (resolution == "" || size == resolution))
	})
	if err == nil && state.OSServices.VideoError != "" {
		err = fmt.Errorf("video: the KVM reports %q", state.OSServices.VideoError)
	}
	return state, err
}

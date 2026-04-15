package events

import (
	"sync"
	"testing"
)

type captureSub struct {
	mu     sync.Mutex
	events []Event
}

func (c *captureSub) Handle(e Event) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.events = append(c.events, e)
}

func (c *captureSub) snapshot() []Event {
	c.mu.Lock()
	defer c.mu.Unlock()
	out := make([]Event, len(c.events))
	copy(out, c.events)
	return out
}

type panicSub struct{}

func (panicSub) Handle(Event) { panic("boom") }

func TestBus_FanOut(t *testing.T) {
	bus := NewBus()
	a := &captureSub{}
	b := &captureSub{}
	bus.Subscribe(a)
	bus.Subscribe(b)

	bus.Publish(Event{Type: TypeRunStarted, RunID: "r1"})
	bus.Publish(Event{Type: TypeRunCompleted, RunID: "r1"})

	if got := a.snapshot(); len(got) != 2 {
		t.Errorf("subscriber a: got %d events, want 2", len(got))
	}
	if got := b.snapshot(); len(got) != 2 {
		t.Errorf("subscriber b: got %d events, want 2", len(got))
	}
}

func TestBus_PanicIsolation(t *testing.T) {
	bus := NewBus()
	bus.Subscribe(panicSub{})
	good := &captureSub{}
	bus.Subscribe(good)

	// Should not panic, and the second subscriber must still receive.
	bus.Publish(Event{Type: TypeTaskClaimed, TaskID: "t1"})

	if got := good.snapshot(); len(got) != 1 {
		t.Errorf("good subscriber: got %d events, want 1", len(got))
	}
}

func TestNopPublisher(t *testing.T) {
	p := NopPublisher{}
	// Must not panic.
	p.Publish(Event{Type: TypeRunStarted})
}

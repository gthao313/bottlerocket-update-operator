package agent

import "github.com/bottlerocket-os/bottlerocket-update-operator/pkg/platform"

type progression struct {
	target platform.Update
}

func (p *progression) SetTarget(t platform.Update) {
	p.target = t
}

func (p *progression) GetTarget() platform.Update {
	return p.target
}

func (p *progression) Reset() {
	p.target = nil
}

func (p *progression) Valid() bool {
	return p.target != nil
}

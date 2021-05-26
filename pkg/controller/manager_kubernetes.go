package controller

import (
	"github.com/bottlerocket-os/bottlerocket-update-operator/pkg/intent"
	"github.com/bottlerocket-os/bottlerocket-update-operator/pkg/k8sutil"
	"github.com/bottlerocket-os/bottlerocket-update-operator/pkg/logging"
	"github.com/pkg/errors"
	"github.com/sirupsen/logrus"
	v1 "k8s.io/api/core/v1"
	v1meta "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
	corev1 "k8s.io/client-go/kubernetes/typed/core/v1"
	"k8s.io/kubectl/pkg/drain"
)

type k8sNodeManager struct {
	kube kubernetes.Interface
}

func (k *k8sNodeManager) forNode(nodeName string) (*v1.Node, *drain.Helper, error) {
	var drainer *drain.Helper
	node, err := k.kube.CoreV1().Nodes().Get(nodeName, v1meta.GetOptions{})
	if err != nil {
		return nil, nil, errors.WithMessage(err, "unable to retrieve node from api")
	}
	drainer = &drain.Helper{Client: k.kube}
	return node, drainer, err
}

func (k *k8sNodeManager) setCordon(nodeName string, cordoned bool) error {
	node, drainer, err := k.forNode(nodeName)
	if err != nil {
		return errors.WithMessage(err, "unable to operate")
	}
	return drain.RunCordonOrUncordon(drainer, node, cordoned)
}

func (k *k8sNodeManager) Uncordon(nodeName string) error {
	return k.setCordon(nodeName, false)
}

func (k *k8sNodeManager) Cordon(nodeName string) error {
	return k.setCordon(nodeName, true)
}

func (k *k8sNodeManager) Drain(nodeName string) error {
	_, drainer, err := k.forNode(nodeName)
	if err != nil {
		return errors.WithMessage(err, "unable to operate")
	}
	return drain.RunNodeDrain(drainer, nodeName)
}

func (am *actionManager) checkNode(nodeName string) error {
	return nil
}

type k8sPoster struct {
	log        logging.Logger
	nodeclient corev1.NodeInterface
}

func (k *k8sPoster) Post(i *intent.Intent) error {
	nodeName := i.GetName()
	err := k8sutil.PostMetadata(k.nodeclient, nodeName, i)
	if err != nil {
		return err
	}
	k.log.WithFields(logrus.Fields{
		"node":   nodeName,
		"intent": i.DisplayString(),
	}).Debugf("posted intent")
	return nil
}

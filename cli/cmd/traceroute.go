package cmd

import (
	"encoding/json"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/linkerd/linkerd2-proxy-api/go/destination"
	"github.com/linkerd/linkerd2-proxy-api/go/http_route"
	"github.com/linkerd/linkerd2-proxy-api/go/inbound"
	"github.com/linkerd/linkerd2-proxy-api/go/meta"
	"github.com/linkerd/linkerd2-proxy-api/go/outbound"
	pkgcmd "github.com/linkerd/linkerd2/pkg/cmd"
	"github.com/linkerd/linkerd2/pkg/k8s"
	"github.com/spf13/cobra"
	"go.opencensus.io/plugin/ocgrpc"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

const (
	destinationPort       = 8086
	destinationDeployment = "linkerd-destination"
)

func newCmdTrace() *cobra.Command {
	options := newEndpointsOptions()
	var (
		namespace = "default"
		output    = ""
	)

	example := `  # get the inbound policy for pod emoji-6d66d87995-bvrnn on port 8080
  linkerd diagnostics policy -n emojivoto po/emoji-6d66d87995-bvrnn 8080

  # get the outbound policy for Service emoji-svc on port 8080
  linkerd diagnostics policy -n emojivoto svc/emoji-svc 8080`

	cmd := &cobra.Command{
		Use:   "trace [flags] resource port",
		Short: "Introspect Linkerd's policy state",
		Long: `Introspect Linkerd's policy state.

This command provides debug information about the internal state of the
control-plane's policy controller. It queries the same control-plane
endpoint as the linkerd-proxy's, and returns the policies associated with the
given resource. If the resource is a Pod, inbound policy for that Pod is
displayed. If the resource is a Service, outbound policy for that Service is
displayed.`,
		Example: example,
		Args:    cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			err := options.validate()
			if err != nil {
				return err
			}

			k8sAPI, err := k8s.NewAPI(kubeconfigPath, kubeContext, impersonate, impersonateGroup, 0)
			if err != nil {
				return err
			}

			elems := strings.Split(args[0], "/")

			if len(elems) == 1 {
				return errors.New("resource type and name are required")
			}

			if len(elems) != 2 {
				return fmt.Errorf("invalid resource string: %s", args[0])
			}
			typ, err := k8s.CanonicalResourceNameFromFriendlyName(elems[0])
			if err != nil {
				return err
			}
			name := elems[1]

			port, err := strconv.ParseUint(args[1], 10, 32)
			if err != nil {
				return err
			}

			if typ != k8s.Service {
				return fmt.Errorf("invalid resource type %s; must be one of Pod or Service", args[0])
			}

			policyAddr := strings.Clone(apiAddr)
			policyPortForward, err := setPolicyApiAddr(cmd, options, k8sAPI, &policyAddr)
			if policyPortForward != nil {
				defer policyPortForward.Stop()
			}
			if err != nil {
				return err
			}

			_, err = getPolicyForService(cmd, options, policyAddr, name, namespace, port)
			if err != nil {
				return err
			}

			destinationAddr := strings.Clone(apiAddr)
			destinationPortForward, err := setDestinationApiAddr(cmd, options, k8sAPI, &destinationAddr)
			if destinationPortForward != nil {
				defer destinationPortForward.Stop()
			}

			if err != nil {
				return err
			}
			_, err = getDestinationForService(cmd, options, destinationAddr, name, namespace, port)
			if err != nil {
				return err
			}

			// var out []byte
			// switch output {
			// case "":
			// 	out = []byte{}
			// case "json":
			// 	out, err = json.MarshalIndent(result, "", "  ")
			// 	if err != nil {
			// 		fmt.Fprint(os.Stderr, err)
			// 		os.Exit(1)
			// 	}
			// case "yaml":
			// 	out, err = yaml.Marshal(result)
			// 	if err != nil {
			// 		fmt.Fprint(os.Stderr, err)
			// 		os.Exit(1)
			// 	}
			// default:
			// 	return errors.New("output must be one of: yaml, json")
			// }

			return nil
		},
	}

	cmd.PersistentFlags().StringVar(&options.destinationPod, "destination-pod", "", "Target a specific destination Pod when there are multiple running")
	cmd.PersistentFlags().StringVar(&options.contextToken, "token", "default:diagnostics", "Token to use when querying the policy service")
	cmd.PersistentFlags().StringVarP(&namespace, "namespace", "n", namespace, "Namespace of resource")
	cmd.PersistentFlags().StringVarP(&output, "output", "o", output, "Output format. One of: <blank>, yaml, json")

	pkgcmd.ConfigureOutputFlagCompletion(cmd)

	return cmd
}

func setPolicyApiAddr(cmd *cobra.Command, options *endpointsOptions, k8sAPI *k8s.KubernetesAPI, addr *string) (*k8s.PortForward, error) {
	var portForward *k8s.PortForward
	if *addr == "" {
		var err error
		if options.destinationPod == "" {
			portForward, err = k8s.NewPortForward(
				cmd.Context(),
				k8sAPI,
				controlPlaneNamespace,
				policyDeployment,
				"localhost",
				0,
				policyPort,
				false,
			)
		} else {
			portForward, err = k8s.NewPodPortForward(k8sAPI, controlPlaneNamespace, options.destinationPod, "localhost", 0, policyPort, false)
		}
		if err != nil {
			return portForward, err
		}

		*addr = portForward.AddressAndPort()
		if err = portForward.Init(); err != nil {
			return portForward, err
		}
	}
	return portForward, nil
}

func getPolicyForService(cmd *cobra.Command, options *endpointsOptions, addr string, name string, namespace string, port uint64) (*outbound.OutboundPolicy, error) {
	conn, err := grpc.NewClient(addr, grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithStatsHandler(&ocgrpc.ClientHandler{}))
	if err != nil {
		return nil, err
	}
	defer conn.Close()

	policyClient := outbound.NewOutboundPoliciesClient(conn)

	outboundResult, err := policyClient.Get(cmd.Context(), &outbound.TrafficSpec{
		SourceWorkload: options.contextToken,
		Target:         &outbound.TrafficSpec_Authority{Authority: fmt.Sprintf("%s.%s.svc:%d", name, namespace, port)},
	})
	if err != nil {
		return nil, nil
	}

	return outboundResult, nil
}

func processOutboundPolicy(outboundResult *outbound.OutboundPolicy) error {
	// _, err := fmt.Printf("outbound: %s\n", outboundResult)
	j, err := json.MarshalIndent(outboundResult, "", "  ")
	if err != nil {
		return err
	}
	_, err = fmt.Printf("%s\n", string(j))
	if err != nil {
		return err
	}

	switch protocol := outboundResult.Protocol.Kind.(type) {
	case *outbound.ProxyProtocol_Detect_:
		timeout := protocol.Detect.Timeout
		_ = timeout // ignore
		for _, route := range protocol.Detect.Opaque.Routes {
			_ = route
		}
		for _, route := range protocol.Detect.Http1.Routes {
			_ = route
		}
		for _, route := range protocol.Detect.Http2.Routes {
			_ = route
		}
	case *outbound.ProxyProtocol_Grpc_:
		for _, route := range protocol.Grpc.Routes {
			meta := route.Metadata
			_ = meta
			for _, host := range route.Hosts {
				switch match := host.Match.(type) {
				case *http_route.HostMatch_Exact:
					_ = match.Exact
				case *http_route.HostMatch_Suffix_:
					_ = match.Suffix.ReverseLabels
				}
			}
			for _, rule := range route.Rules {
				_ = rule.Matches
				_ = rule.Filters
				_ = rule.Backends
				_ = rule.Timeouts
				_ = rule.Retry
				_ = rule.AllowL5DRequestHeaders
			}
		}
	case *outbound.ProxyProtocol_Http1_:
		for _, route := range protocol.Http1.Routes {
			meta := route.Metadata
			_ = meta
			for _, host := range route.Hosts {
				switch match := host.Match.(type) {
				case *http_route.HostMatch_Exact:
					_ = match.Exact
				case *http_route.HostMatch_Suffix_:
					_ = match.Suffix.ReverseLabels
				}
			}
			for _, rule := range route.Rules {
				_ = rule.Matches
				_ = rule.Filters
				_ = rule.Backends
				_ = rule.Timeouts
				_ = rule.Retry
				_ = rule.AllowL5DRequestHeaders
			}
		}
	case *outbound.ProxyProtocol_Http2_:
		for _, route := range protocol.Http2.Routes {
			meta := route.Metadata
			_ = meta
			for _, host := range route.Hosts {
				switch match := host.Match.(type) {
				case *http_route.HostMatch_Exact:
					_ = match.Exact
				case *http_route.HostMatch_Suffix_:
					_ = match.Suffix.ReverseLabels
				}
			}
			for _, rule := range route.Rules {
				_ = rule.Matches
				_ = rule.Filters
				_ = rule.Backends
				_ = rule.Timeouts
				_ = rule.Retry
				_ = rule.AllowL5DRequestHeaders
			}
		}
	case *outbound.ProxyProtocol_Opaque_:
		for _, route := range protocol.Opaque.Routes {
			meta := route.Metadata
			_ = meta
			for _, rule := range route.Rules {
				_ = rule.Backends
			}
		}
	}
	switch metadata := outboundResult.Metadata.Kind.(type) {
	case *meta.Metadata_Default:
		_ = metadata
	case *meta.Metadata_Resource:
		_ = metadata
	}

	return nil
}

func processInboundPolicy(inboundResult *inbound.Server) error {
	_, err := fmt.Printf("inbound: %s\n", inboundResult)
	if err != nil {
		return err
	}

	_ = inboundResult.Protocol
	for _, ip := range inboundResult.ServerIps {
		_ = ip
	}
	for _, authz := range inboundResult.Authorizations {
		_ = authz
	}
	_ = inboundResult.Labels

	return nil
}

func setDestinationApiAddr(cmd *cobra.Command, options *endpointsOptions, k8sAPI *k8s.KubernetesAPI, addr *string) (*k8s.PortForward, error) {
	var portForward *k8s.PortForward
	if *addr == "" {
		var err error
		if options.destinationPod == "" {
			portForward, err = k8s.NewPortForward(
				cmd.Context(),
				k8sAPI,
				controlPlaneNamespace,
				destinationDeployment,
				"localhost",
				0,
				destinationPort,
				false,
			)
		} else {
			portForward, err = k8s.NewPodPortForward(k8sAPI, controlPlaneNamespace, options.destinationPod, "localhost", 0, destinationPort, false)
		}
		if err != nil {
			return portForward, err
		}

		*addr = portForward.AddressAndPort()
		if err = portForward.Init(); err != nil {
			return portForward, err
		}
	}
	return portForward, nil
}

func getDestinationForService(cmd *cobra.Command, options *endpointsOptions, addr string, name string, namespace string, port uint64) ([]*destination.Update, error) {
	conn, err := grpc.NewClient(addr, grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithStatsHandler(&ocgrpc.ClientHandler{}))
	if err != nil {
		return nil, err
	}
	defer conn.Close()

	destinationClient := destination.NewDestinationClient(conn)

	stream, err := destinationClient.Get(cmd.Context(), &destination.GetDestination{
		Scheme:       "http:",
		Path:         fmt.Sprintf("%s.%s.svc.cluster.local:%d", name, namespace, port),
		ContextToken: "diagnostics",
	})
	if err != nil {
		return nil, err
	}

	c := make(chan UpdateMsg, 5)
	go recvDesinationUpdates(stream, c)
	out := make([]*destination.Update, 0)
	for i := range c {
		if i.err != nil {
			fmt.Printf("%s\n", i.update)
			return nil, i.err
		}
		out = append(out, i.update)
	}

	return out, nil
}

type UpdateMsg struct {
	update *destination.Update
	err    error
}

func recvDesinationUpdates(stream destination.Destination_GetClient, channel chan UpdateMsg) {
	c := make(chan UpdateMsg, 1)

out:
	for {
		go func() {
			msg, err := stream.Recv()
			c <- UpdateMsg{
				update: msg,
				err:    err,
			}
		}()

		select {
		case msg := <-c:
			channel <- msg
		case <-time.After(1 * time.Second):
			close(channel)
			break out
		}
	}
}

type tracerouteNode struct {
	content  string
	labels   map[string]string
	children []*tracerouteNode
}

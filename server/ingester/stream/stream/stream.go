/*
 * Copyright (c) 2022 Yunshan Networks
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package stream

import (
	"net"
	"strconv"
	"time"

	_ "golang.org/x/net/context"
	_ "google.golang.org/grpc"

	dropletqueue "github.com/deepflowys/deepflow/server/ingester/droplet/queue"
	"github.com/deepflowys/deepflow/server/ingester/flow_tag"
	"github.com/deepflowys/deepflow/server/ingester/ingesterctl"
	"github.com/deepflowys/deepflow/server/ingester/stream/common"
	"github.com/deepflowys/deepflow/server/ingester/stream/config"
	"github.com/deepflowys/deepflow/server/ingester/stream/dbwriter"
	"github.com/deepflowys/deepflow/server/ingester/stream/decoder"
	"github.com/deepflowys/deepflow/server/ingester/stream/geo"
	"github.com/deepflowys/deepflow/server/ingester/stream/throttler"
	"github.com/deepflowys/deepflow/server/libs/datatype"
	"github.com/deepflowys/deepflow/server/libs/debug"
	"github.com/deepflowys/deepflow/server/libs/grpc"
	"github.com/deepflowys/deepflow/server/libs/queue"
	libqueue "github.com/deepflowys/deepflow/server/libs/queue"
	"github.com/deepflowys/deepflow/server/libs/receiver"
)

const (
	CMD_PLATFORMDATA = 34
)

type Stream struct {
	StreamConfig         *config.Config
	L4FlowLogger         *Logger
	L7FlowLogger         *Logger
	OtelLogger           *Logger
	OtelCompressedLogger *Logger
	L4PacketLogger       *Logger
}

type Logger struct {
	Config        *config.Config
	Decoders      []*decoder.Decoder
	PlatformDatas []*grpc.PlatformInfoTable
	FlowLogWriter *dbwriter.FlowLogWriter
}

func NewStream(config *config.Config, recv *receiver.Receiver) (*Stream, error) {
	manager := dropletqueue.NewManager(ingesterctl.INGESTERCTL_STREAM_QUEUE)
	controllers := make([]net.IP, len(config.Base.ControllerIPs))
	for i, ipString := range config.Base.ControllerIPs {
		controllers[i] = net.ParseIP(ipString)
		if controllers[i].To4() != nil {
			controllers[i] = controllers[i].To4()
		}
	}
	geo.NewGeoTree()

	flowLogWriter, err := dbwriter.NewFlowLogWriter(
		config.Base.CKDB.ActualAddr, config.Base.CKDBAuth.Username, config.Base.CKDBAuth.Password,
		config.Base.CKDB.ClusterName, config.Base.CKDB.StoragePolicy,
		config.CKWriterConfig, config.FlowLogTTL, config.Base.GetCKDBColdStorages())
	if err != nil {
		return nil, err
	}
	l4FlowLogger := NewL4FlowLogger(config, controllers, manager, recv, flowLogWriter)
	flowTagWriter, err := flow_tag.NewFlowTagWriter(common.FLOW_LOG_DB, common.FLOW_LOG_DB, config.FlowLogTTL.L7FlowLog, dbwriter.DefaultPartition, config.Base, &config.CKWriterConfig)
	if err != nil {
		return nil, err
	}
	l7FlowLogger := NewL7FlowLogger(config, controllers, manager, recv, flowLogWriter, flowTagWriter)
	otelLogger := NewLogger(datatype.MESSAGE_TYPE_OPENTELEMETRY, config, controllers, manager, recv, flowLogWriter, common.L7_FLOW_ID, flowTagWriter)
	otelCompressedLogger := NewLogger(datatype.MESSAGE_TYPE_OPENTELEMETRY_COMPRESSED, config, controllers, manager, recv, flowLogWriter, common.L7_FLOW_ID, flowTagWriter)
	l4PacketLogger := NewLogger(datatype.MESSAGE_TYPE_PACKETSEQUENCE, config, nil, manager, recv, flowLogWriter, common.L4_PACKET_ID, nil)
	return &Stream{
		StreamConfig:         config,
		L4FlowLogger:         l4FlowLogger,
		L7FlowLogger:         l7FlowLogger,
		OtelLogger:           otelLogger,
		OtelCompressedLogger: otelCompressedLogger,
		L4PacketLogger:       l4PacketLogger,
	}, nil
}

func NewLogger(msgType datatype.MessageType, config *config.Config, controllers []net.IP, manager *dropletqueue.Manager, recv *receiver.Receiver, flowLogWriter *dbwriter.FlowLogWriter, flowLogId common.FlowLogID, flowTagWriter *flow_tag.FlowTagWriter) *Logger {
	queueCount := config.DecoderQueueCount
	decodeQueues := manager.NewQueues(
		"1-receive-to-decode-"+datatype.MessageTypeString[msgType],
		config.DecoderQueueSize,
		queueCount,
		1,
		libqueue.OptionFlushIndicator(3*time.Second),
		libqueue.OptionRelease(func(p interface{}) { receiver.ReleaseRecvBuffer(p.(*receiver.RecvBuffer)) }))
	recv.RegistHandler(msgType, decodeQueues, queueCount)
	throttle := config.Throttle / queueCount

	throttlers := make([]*throttler.ThrottlingQueue, queueCount)
	decoders := make([]*decoder.Decoder, queueCount)
	platformDatas := make([]*grpc.PlatformInfoTable, queueCount)
	for i := 0; i < queueCount; i++ {
		throttlers[i] = throttler.NewThrottlingQueue(
			throttle,
			flowLogWriter,
			int(flowLogId),
		)
		if controllers != nil {
			platformDatas[i] = grpc.NewPlatformInfoTable(controllers, int(config.Base.ControllerPort), config.Base.GrpcBufferSize, "stream-"+datatype.MessageTypeString[msgType]+"-"+strconv.Itoa(i), "", config.Base.NodeIP, nil)
			if i == 0 {
				debug.ServerRegisterSimple(CMD_PLATFORMDATA, platformDatas[i])
			}
		}
		decoders[i] = decoder.NewDecoder(
			i,
			msgType,
			platformDatas[i],
			queue.QueueReader(decodeQueues.FixedMultiQueue[i]),
			throttlers[i],
			flowTagWriter,
		)
	}
	return &Logger{
		Config:        config,
		Decoders:      decoders,
		PlatformDatas: platformDatas,
		FlowLogWriter: flowLogWriter,
	}
}

func NewL4FlowLogger(config *config.Config, controllers []net.IP, manager *dropletqueue.Manager, recv *receiver.Receiver, flowLogWriter *dbwriter.FlowLogWriter) *Logger {
	msgType := datatype.MESSAGE_TYPE_TAGGEDFLOW
	queueCount := config.DecoderQueueCount
	queueSuffix := "-l4"
	decodeQueues := manager.NewQueues(
		"1-receive-to-decode"+queueSuffix,
		config.DecoderQueueSize,
		queueCount,
		1,
		libqueue.OptionFlushIndicator(3*time.Second),
		libqueue.OptionRelease(func(p interface{}) { receiver.ReleaseRecvBuffer(p.(*receiver.RecvBuffer)) }))

	recv.RegistHandler(msgType, decodeQueues, queueCount)

	throttle := config.Throttle / queueCount
	if config.L4Throttle != 0 {
		throttle = config.L4Throttle / queueCount
	}

	throttlers := make([]*throttler.ThrottlingQueue, queueCount)
	decoders := make([]*decoder.Decoder, queueCount)
	platformDatas := make([]*grpc.PlatformInfoTable, queueCount)

	for i := 0; i < queueCount; i++ {
		throttlers[i] = throttler.NewThrottlingQueue(
			throttle,
			flowLogWriter,
			int(common.L4_FLOW_ID),
		)
		platformDatas[i] = grpc.NewPlatformInfoTable(controllers, int(config.Base.ControllerPort), config.Base.GrpcBufferSize, "stream-l4-log-"+strconv.Itoa(i), "", config.Base.NodeIP, nil)
		if i == 0 {
			debug.ServerRegisterSimple(CMD_PLATFORMDATA, platformDatas[i])
		}
		decoders[i] = decoder.NewDecoder(
			i,
			msgType,
			platformDatas[i],
			queue.QueueReader(decodeQueues.FixedMultiQueue[i]),
			throttlers[i],
			nil,
		)
	}
	return &Logger{
		Config:        config,
		Decoders:      decoders,
		PlatformDatas: platformDatas,
		FlowLogWriter: flowLogWriter,
	}
}

func NewL7FlowLogger(config *config.Config, controllers []net.IP, manager *dropletqueue.Manager, recv *receiver.Receiver, flowLogWriter *dbwriter.FlowLogWriter, flowTagWriter *flow_tag.FlowTagWriter) *Logger {
	queueSuffix := "-l7"
	queueCount := config.DecoderQueueCount
	msgType := datatype.MESSAGE_TYPE_PROTOCOLLOG

	decodeQueues := manager.NewQueues(
		"1-receive-to-decode"+queueSuffix,
		config.DecoderQueueSize,
		queueCount,
		1,
		libqueue.OptionFlushIndicator(3*time.Second),
		libqueue.OptionRelease(func(p interface{}) { receiver.ReleaseRecvBuffer(p.(*receiver.RecvBuffer)) }))

	recv.RegistHandler(msgType, decodeQueues, queueCount)

	throttle := config.Throttle / queueCount
	if config.L7Throttle != 0 {
		throttle = config.L7Throttle / queueCount
	}

	throttlers := make([]*throttler.ThrottlingQueue, queueCount)

	platformDatas := make([]*grpc.PlatformInfoTable, queueCount)
	decoders := make([]*decoder.Decoder, queueCount)
	for i := 0; i < queueCount; i++ {
		throttlers[i] = throttler.NewThrottlingQueue(
			throttle,
			flowLogWriter,
			int(common.L7_FLOW_ID),
		)
		platformDatas[i] = grpc.NewPlatformInfoTable(controllers, int(config.Base.ControllerPort), config.Base.GrpcBufferSize, "stream-l7-log-"+strconv.Itoa(i), "", config.Base.NodeIP, nil)
		decoders[i] = decoder.NewDecoder(
			i,
			msgType,
			platformDatas[i],
			queue.QueueReader(decodeQueues.FixedMultiQueue[i]),
			throttlers[i],
			flowTagWriter,
		)
	}

	return &Logger{
		Config:        config,
		Decoders:      decoders,
		PlatformDatas: platformDatas,
	}
}

func (l *Logger) Start() {
	for _, platformData := range l.PlatformDatas {
		if platformData != nil {
			platformData.Start()
		}
	}

	for _, decoder := range l.Decoders {
		go decoder.Run()
	}
}

func (l *Logger) Close() {
	for _, platformData := range l.PlatformDatas {
		if platformData != nil {
			platformData.Close()
		}
	}
}

func (s *Stream) Start() {
	s.L4FlowLogger.Start()
	s.L7FlowLogger.Start()
	s.L4PacketLogger.Start()
	s.OtelLogger.Start()
	s.OtelCompressedLogger.Start()
}

func (s *Stream) Close() error {
	s.L4FlowLogger.Close()
	s.L7FlowLogger.Close()
	s.L4PacketLogger.Close()
	s.OtelLogger.Close()
	s.OtelCompressedLogger.Close()
	return nil
}

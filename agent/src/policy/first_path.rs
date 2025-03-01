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

use std::collections::HashMap;
use std::sync::Arc;

use log::warn;

use super::fast_path::FastPath;
use super::Result as PResult;
use super::UnsafeWrapper;
use crate::common::endpoint::{EndpointData, FeatureFlags};
use crate::common::feature;
use crate::common::lookup_key::LookupKey;
use crate::common::matched_field::{MatchedField, MatchedFieldN, MatchedFieldv4, MatchedFieldv6};
use crate::common::platform_data::PlatformData;
use crate::common::policy::{Acl, Cidr, Fieldv4, Fieldv6, IpGroupData, IpSegment};
use npb_pcap_policy::{DirectionType, PolicyData};

struct Vector<const N: usize> {
    min_bit: usize,
    max_bit: usize,
    count: usize,
    mask: MatchedFieldN<N>,
    vector_bits: Vec<usize>,
}

impl<const N: usize> Default for Vector<N> {
    fn default() -> Self {
        Self {
            min_bit: 0,
            max_bit: 0,
            count: 0,
            mask: MatchedFieldN::<N>::default(),
            vector_bits: vec![],
        }
    }
}

type Vector6 = Vector<16>;
type Vector4 = Vector<4>;

impl<const N: usize> Vector<N> {
    // 初始索引，当比特位越能均分策略该值越小，例如：
    // +-----------+-----------+------+--------+
    // | matched_0 | matched_1 | base | result |
    // -----------------------------------------
    // | 0         | 0         | 10   | 10     |
    // | 5         | 5         | 10   | 0      |
    // | 3         | 3         | 10   | 4      |
    // | 4         | 5         | 10   | 2      |
    // | 1         | 9         | 10   | 8      |
    // -----------------------------------------
    fn abs_diff(a: usize, b: usize) -> usize {
        if a > b {
            a - b
        } else {
            b - a
        }
    }

    fn calc_index(matched_0: usize, matched_1: usize, base: usize) -> usize {
        if matched_0 == 0 && matched_1 == 0 {
            return base;
        }
        return Self::abs_diff(matched_0, matched_1) + (base - (matched_0 + matched_1));
    }

    fn sort_table_index(matched_0: usize, matched_1: usize, base: usize) -> usize {
        let mut index = Self::calc_index(matched_0, matched_1, base);
        if index > i16::MAX as usize {
            // 当index非常大时我们需要建立一个多对一的映射关系将其映射到数组的后32767位中
            //
            // 数组前部分存未映射的数据，数组后部分存映射的数据
            let n = (base >> 15) + 1;
            index = (index / n) + i16::MAX as usize;
        }
        return index;
    }

    fn generate_sort_table4(&self, acls: &Vec<Acl>, base: usize) -> Vec<Vec<usize>> {
        let mut table: Vec<Vec<usize>> = std::iter::repeat(Vec::new())
            .take(u16::MAX as usize)
            .collect();
        for i in 0..self.mask.bit_size() {
            let mut matched_0 = 0;
            let mut matched_1 = 0;

            for acl in acls {
                for v4 in &acl.match_field {
                    if v4.mask.is_bit_zero(i) {
                        continue;
                    }

                    if v4.field.is_bit_zero(i) {
                        matched_0 += 1;
                    } else {
                        matched_1 += 1;
                    }
                }
            }
            let index = Self::sort_table_index(matched_0, matched_1, base);
            table[index].push(i);
        }
        return table;
    }

    fn generate_sort_table6(&self, acls: &Vec<Acl>, base: usize) -> Vec<Vec<usize>> {
        let mut table: Vec<Vec<usize>> = std::iter::repeat(Vec::new())
            .take(u16::MAX as usize)
            .collect();
        for i in 0..self.mask.bit_size() {
            let mut matched_0 = 0;
            let mut matched_1 = 0;

            for acl in acls {
                for v6 in &acl.match_field6 {
                    if v6.mask.is_bit_zero(i) {
                        continue;
                    }

                    if v6.field.is_bit_zero(i) {
                        matched_0 += 1;
                    } else {
                        matched_1 += 1;
                    }
                }
            }
            let index = Self::sort_table_index(matched_0, matched_1, base);
            table[index].push(i);
        }
        return table;
    }

    fn generate_sort_table(&self, acls: &Vec<Acl>) -> Vec<Vec<usize>> {
        let mut base = 0;
        let is_ipv6 = self.mask.is_ipv6();
        acls.iter().for_each(|x| {
            base += if is_ipv6 {
                x.match_field6.len()
            } else {
                x.match_field.len()
            }
        });

        if !is_ipv6 {
            return self.generate_sort_table4(acls, base);
        }
        return self.generate_sort_table6(acls, base);
    }

    fn init(&mut self, acls: &Vec<Acl>, vector_size: usize) {
        let table = self.generate_sort_table(acls);
        let mut vector_bits = Vec::new();
        for i in 0..u16::MAX as usize {
            for bit_offset in &table[i] {
                vector_bits.push(*bit_offset);
                if vector_bits.len() == u16::BITS as usize {
                    break;
                }
            }
            if vector_bits.len() == u16::BITS as usize {
                break;
            }
        }
        vector_bits.sort();
        self.min_bit = vector_bits[0];
        self.max_bit = vector_bits[vector_size - 1];
        self.mask.set_bits(&vector_bits);
        self.vector_bits = vector_bits;
    }
}

type Table4 = UnsafeWrapper<Vec<Vec<Table4Item>>>;

#[derive(Clone, Debug)]
struct Table4Item {
    field: Arc<Fieldv4>,
    policy: PolicyData,
}

type Table6 = UnsafeWrapper<Vec<Vec<Table6Item>>>;

#[derive(Clone, Debug)]
struct Table6Item {
    field: Arc<Fieldv6>,
    policy: PolicyData,
}

pub struct FirstPath {
    group_ip_map: Option<HashMap<u16, Vec<IpSegment>>>,

    vector_4: Vector4,
    table_4: Table4,
    vector_6: Vector6,
    table_6: Table6,

    level: usize,

    fast: FastPath,

    fast_disable: bool,
    queue_count: usize,

    features: feature::FeatureFlags,
}

impl FirstPath {
    const VECTOR_MASK_SIZE_MAX: usize = 16;
    const VECTOR_MASK_SIZE_MIN: usize = 4;
    const LEVEL_MIN: usize = 1;
    const LEVEL_MAX: usize = 16;
    const TABLE_SIZE: usize = 1 << Self::VECTOR_MASK_SIZE_MAX;

    pub fn new(
        queue_count: usize,
        level: usize,
        map_size: usize,
        fast_disable: bool,
        features: feature::FeatureFlags,
    ) -> FirstPath {
        FirstPath {
            group_ip_map: Some(HashMap::new()),
            vector_4: Vector4::default(),
            table_4: Table4::from(
                std::iter::repeat(Vec::new())
                    .take(Self::TABLE_SIZE)
                    .collect::<Vec<Vec<Table4Item>>>(),
            ),
            vector_6: Vector6::default(),
            table_6: Table6::from(
                std::iter::repeat(Vec::new())
                    .take(Self::TABLE_SIZE)
                    .collect::<Vec<Vec<Table6Item>>>(),
            ),
            level,

            fast: FastPath::new(queue_count, map_size),
            queue_count,
            fast_disable,
            features,
        }
    }

    pub fn update_map_size(&mut self, map_size: usize) {
        self.fast.update_map_size(map_size)
    }

    pub fn update_interfaces(&mut self, ifaces: &Vec<Arc<PlatformData>>) {
        self.fast.generate_mask_from_interface(ifaces);
        self.fast.generate_mask_table();
    }

    fn generate_group_ip_map(&mut self, groups: &Vec<Arc<IpGroupData>>) {
        let mut group_ip_map: HashMap<u16, Vec<IpSegment>> = HashMap::new();

        for group in groups {
            if group.id == 0 {
                continue;
            }

            for ip in &group.ips {
                let mut ip_segment = IpSegment::from(ip);
                ip_segment.set_epc_id(group.epc_id);
                if let Some(segments) = group_ip_map.get_mut(&group.id) {
                    segments.push(ip_segment);
                } else {
                    group_ip_map.insert(group.id, vec![ip_segment]);
                }
            }
        }

        self.group_ip_map.replace(group_ip_map);
    }

    pub fn update_ip_group(&mut self, groups: &Vec<Arc<IpGroupData>>) {
        if self.features.contains(feature::FeatureFlags::POLICY) {
            self.generate_group_ip_map(groups);
        }

        self.fast.generate_mask_table_from_group(groups);
        self.fast.generate_mask_table();
    }

    pub fn update_cidr(&mut self, cidrs: &Vec<Arc<Cidr>>) {
        self.fast.generate_mask_table_from_cidr(cidrs);
        self.fast.generate_mask_table();
    }

    fn is_invalid_acl(&self, acl: &Arc<Acl>, check: bool) -> bool {
        if !check {
            return false;
        }

        if self.group_ip_map.is_none() {
            warn!("IpGroup is nil, invalid acl: {}", acl);
            return false;
        }

        for group in &acl.src_groups {
            if self
                .group_ip_map
                .as_ref()
                .unwrap()
                .get(&(*group as u16))
                .is_none()
            {
                warn!("Invalid acl by src group({}): {}", group, acl);
                return true;
            }
        }

        for group in &acl.dst_groups {
            if self
                .group_ip_map
                .as_ref()
                .unwrap()
                .get(&(*group as u16))
                .is_none()
            {
                warn!("Invalid acl by dst group({}): {}", group, acl);
                return true;
            }
        }
        return false;
    }

    fn generate_acl_bits(&mut self, acls: &mut Vec<Acl>) -> PResult<u64> {
        let memory = 0;
        for acl in acls {
            let mut src_ips = Vec::new();
            let mut dst_ips = Vec::new();

            for group in &acl.src_groups {
                for ip_segment in self
                    .group_ip_map
                    .as_ref()
                    .unwrap()
                    .get(&(*group as u16))
                    .unwrap()
                {
                    src_ips.push(ip_segment.clone());
                }
            }
            for group in &acl.dst_groups {
                for ip_segment in self
                    .group_ip_map
                    .as_ref()
                    .unwrap()
                    .get(&(*group as u16))
                    .unwrap()
                {
                    dst_ips.push(ip_segment.clone());
                }
            }

            if src_ips.is_empty() {
                src_ips.append(&mut vec![IpSegment::IPV4_ANY, IpSegment::IPV6_ANY]);
            }
            if dst_ips.is_empty() {
                dst_ips.append(&mut vec![IpSegment::IPV4_ANY, IpSegment::IPV6_ANY]);
            }

            acl.generate_match(&src_ips, &dst_ips);
            acl.init_policy();
        }

        Ok(memory)
    }

    fn vector_size(&self, acls: &Vec<Acl>, _memory_exceeded: bool) -> usize {
        let mut sum = 0;
        acls.iter()
            .for_each(|x| sum += x.match_field.len() + x.match_field6.len());

        for vector_size in Self::VECTOR_MASK_SIZE_MAX..Self::VECTOR_MASK_SIZE_MIN {
            if sum >> self.level >= 1 << vector_size {
                return vector_size;
            }
        }
        return Self::VECTOR_MASK_SIZE_MIN;
    }

    fn generate_table4(&mut self, acls: &mut Vec<Acl>) -> PResult<()> {
        let mut table_4 = std::iter::repeat(Vec::new())
            .take(Self::TABLE_SIZE)
            .collect::<Vec<Vec<Table4Item>>>();

        for acl in acls {
            for v4 in &acl.match_field {
                for index in v4.field.get_all_table_index(
                    &self.vector_4.mask,
                    &v4.mask,
                    self.vector_4.min_bit,
                    self.vector_4.max_bit,
                    &self.vector_4.vector_bits,
                ) {
                    table_4[index as usize].push(Table4Item {
                        field: v4.clone(),
                        policy: acl.policy.clone(),
                    });
                }
            }
        }

        self.table_4.set(table_4);

        Ok(())
    }

    fn generate_table6(&mut self, acls: &mut Vec<Acl>) -> PResult<()> {
        let mut table_6 = std::iter::repeat(Vec::new())
            .take(Self::TABLE_SIZE)
            .collect::<Vec<Vec<Table6Item>>>();

        for acl in acls {
            for v6 in &acl.match_field6 {
                for index in v6.field.get_all_table_index(
                    &self.vector_6.mask,
                    &v6.mask,
                    self.vector_6.min_bit,
                    self.vector_6.max_bit,
                    &self.vector_6.vector_bits,
                ) {
                    table_6[index as usize].push(Table6Item {
                        field: v6.clone(),
                        policy: acl.policy.clone(),
                    });
                }
            }
        }

        self.table_6.set(table_6);

        Ok(())
    }

    fn generate_first_table(&mut self, acls: &mut Vec<Acl>) -> PResult<()> {
        self.generate_acl_bits(acls)?;

        let vector_size = self.vector_size(acls, false);
        self.vector_4.init(acls, vector_size);
        self.vector_6.init(acls, vector_size);
        self.generate_table4(acls)?;
        self.generate_table6(acls)?;

        Ok(())
    }

    pub fn update_acl(&mut self, acls: &Vec<Arc<Acl>>, check: bool) -> PResult<()> {
        if self.features.contains(feature::FeatureFlags::POLICY) {
            let mut valid_acls = Vec::new();

            for acl in acls {
                if self.is_invalid_acl(acl, check) {
                    continue;
                }
                let mut valid_acl = (**acl).clone();

                valid_acl.reset();
                valid_acls.push(valid_acl);
            }
            self.generate_first_table(&mut valid_acls)?;
        }

        // fast
        self.fast.generate_interest_table(acls);

        Ok(())
    }

    pub fn flush(&mut self) {
        self.fast.flush();
    }

    fn get_policy_from_table4(
        &self,
        field: &MatchedFieldv4,
        direction: DirectionType,
        policy: &mut PolicyData,
    ) {
        let index = field.get_table_index(
            &self.vector_4.mask,
            self.vector_4.min_bit,
            self.vector_4.max_bit,
        ) as usize;
        for item in &self.table_4.get()[index] {
            if field & &item.field.mask == item.field.field {
                policy.merge_npb_action(
                    &item.policy.npb_actions,
                    item.policy.acl_id,
                    Some(direction),
                );
            }
        }
    }

    fn get_policy_from_table6(
        &self,
        field: &MatchedFieldv6,
        direction: DirectionType,
        policy: &mut PolicyData,
    ) {
        let index = field.get_table_index(
            &self.vector_6.mask,
            self.vector_6.min_bit,
            self.vector_6.max_bit,
        ) as usize;
        for item in &self.table_6.get()[index] {
            if field & &item.field.mask == item.field.field {
                policy.merge_npb_action(
                    &item.policy.npb_actions,
                    item.policy.acl_id,
                    Some(direction),
                );
            }
        }
    }

    fn get_policy_from_table(
        &mut self,
        key: &mut LookupKey,
        endpoints: &EndpointData,
        policy: &mut PolicyData,
    ) {
        key.generate_matched_field(
            (endpoints.src_info.l3_epc_id & 0xffff) as u16,
            (endpoints.dst_info.l3_epc_id & 0xffff) as u16,
        );

        match (
            key.forward_matched.as_ref().unwrap(),
            key.backward_matched.as_ref().unwrap(),
        ) {
            (MatchedField::V4(forward), MatchedField::V4(backward)) => {
                self.get_policy_from_table4(forward, DirectionType::Forward, policy);
                self.get_policy_from_table4(backward, DirectionType::Backward, policy);
            }
            (MatchedField::V6(forward), MatchedField::V6(backward)) => {
                self.get_policy_from_table6(forward, DirectionType::Forward, policy);
                self.get_policy_from_table6(backward, DirectionType::Backward, policy);
            }
            _ => panic!("LookupKey MatchedField version error."),
        }
    }

    pub fn first_get(
        &mut self,
        key: &mut LookupKey,
        endpoints: EndpointData,
    ) -> Option<(Arc<PolicyData>, Arc<EndpointData>)> {
        let mut policy = PolicyData::default();

        if self.features.contains(feature::FeatureFlags::POLICY) {
            self.get_policy_from_table(key, &endpoints, &mut policy);
        }

        self.fast.add_policy(key, &policy, endpoints);

        policy.format_npb_action();
        if key.feature_flag.contains(FeatureFlags::DEDUP) {
            policy.dedup(key);
        }

        return Some((Arc::new(policy), Arc::new(endpoints)));
    }

    pub fn fast_get(
        &mut self,
        key: &mut LookupKey,
    ) -> Option<(Arc<PolicyData>, Arc<EndpointData>)> {
        if self.fast_disable {
            return None;
        }
        if let Some((policy, endpoints)) = self.fast.get_policy(key) {
            if key.feature_flag.contains(FeatureFlags::DEDUP) {
                let mut policy = PolicyData {
                    acl_id: policy.acl_id,
                    action_flags: policy.action_flags,
                    npb_actions: policy.npb_actions.clone(),
                };
                policy.dedup(key);
                return Some((Arc::new(policy), endpoints));
            }
            return Some((policy, endpoints));
        }
        return None;
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;
    use crate::common::endpoint::EndpointInfo;
    use crate::common::enums::TapType;
    use crate::common::port_range::PortRange;

    use npb_pcap_policy::{NpbAction, NpbTunnelType, TapSide};

    fn generate_table() -> PResult<FirstPath> {
        let mut first = FirstPath::new(1, 8, 1 << 16, false, feature::FeatureFlags::POLICY);
        let acl = Acl::new(
            1,
            vec![10],
            vec![20],
            vec![PortRange::new(80, 80)],
            vec![PortRange::new(100, 100)],
            NpbAction::new(
                0,
                100,
                "192.168.1.100".parse::<IpAddr>().unwrap(),
                NpbTunnelType::VxLan,
                TapSide::SRC,
                0,
            ),
        );

        first.update_ip_group(&vec![
            Arc::new(IpGroupData::new(10, 2, "192.168.2.1/32")),
            Arc::new(IpGroupData::new(20, 20, "192.168.2.5/31")),
        ]);
        first.update_acl(&vec![Arc::new(acl)], true)?;

        Ok(first)
    }

    #[test]
    fn test_first() {
        let mut first = generate_table().unwrap();
        let endpotins = EndpointData {
            src_info: EndpointInfo {
                l3_epc_id: 2,
                ..Default::default()
            },
            dst_info: EndpointInfo {
                l3_epc_id: 20,
                ..Default::default()
            },
        };
        let mut key = LookupKey {
            src_ip: "192.168.2.1".parse::<IpAddr>().unwrap(),
            dst_ip: "192.168.2.5".parse::<IpAddr>().unwrap(),
            src_port: 80,
            dst_port: 100,
            feature_flag: FeatureFlags::NONE,
            tap_type: TapType::Cloud,
            ..Default::default()
        };

        let result = first.first_get(&mut key, endpotins);
        assert_eq!(result.is_some(), true);

        let (_policy, endpoints) = result.unwrap();
        assert_eq!(endpoints.src_info.l3_epc_id, 2);
        assert_eq!(endpoints.dst_info.l3_epc_id, 20);
        // assert_eq!(policy.npb_actions[0].tunnel_id(), 100);
        assert_eq!(key.src_port, 80);
        assert_eq!(key.dst_port, 100);
    }

    #[test]
    fn test_dedup() {
        let mut first = generate_table().unwrap();
        let mut endpotins = EndpointData {
            src_info: EndpointInfo {
                l3_epc_id: 2,
                ..Default::default()
            },
            dst_info: EndpointInfo {
                l3_epc_id: 20,
                ..Default::default()
            },
        };

        let mut key = LookupKey {
            src_ip: "192.168.2.1".parse::<IpAddr>().unwrap(),
            dst_ip: "192.168.2.5".parse::<IpAddr>().unwrap(),
            src_port: 80,
            dst_port: 100,
            feature_flag: FeatureFlags::DEDUP,
            tap_type: TapType::Cloud,
            ..Default::default()
        };
        let (_policy, _) = first.first_get(&mut key, endpotins).unwrap();
        // assert_eq!(policy.npb_actions.len(), 0);
        // assert_eq!(policy.acl_id, 0);

        key.l2_end_0 = true;
        key.l3_end_0 = true;
        let (policy, _) = first.first_get(&mut key, endpotins).unwrap();
        assert_eq!(policy.npb_actions.len(), 1);
        // assert_eq!(policy.acl_id, 1);

        let (policy, _) = first.fast_get(&mut key).unwrap();
        // assert_eq!(policy.npb_actions.len(), 1);
        assert_eq!(policy.npb_actions.len(), 2);
        assert_eq!(policy.acl_id, 1);

        key.reverse();
        endpotins.src_info.l3_epc_id = 20;
        endpotins.dst_info.l3_epc_id = 2;
        let (policy, _) = first.first_get(&mut key, endpotins).unwrap();
        assert_eq!(policy.npb_actions.len(), 1);
        assert_eq!(policy.acl_id, 1);

        key.l2_end_1 = false;
        key.l3_end_1 = false;
        let (policy, _) = first.first_get(&mut key, endpotins).unwrap();
        // assert_eq!(policy.npb_actions.len(), 0);
        // assert_eq!(policy.acl_id, 0);
        assert_eq!(policy.npb_actions.len(), 1);
        assert_eq!(policy.acl_id, 1);

        let (policy, _) = first.fast_get(&mut key).unwrap();
        // assert_eq!(policy.npb_actions.len(), 0);
        // assert_eq!(policy.acl_id, 0);
        assert_eq!(policy.npb_actions.len(), 2);
        assert_eq!(policy.acl_id, 1);
    }
}

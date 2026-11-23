import { describe, expect, it } from 'vitest'
import {
  buildPlaybackUrl,
  isEndedLiveDetail,
  isLiveEndedMessage,
  isLiveRemovedMessage,
  pickPreferredStream,
  pickPreferredVodStream,
} from '@renderer/utils/live-stream'
import { formatMediaTime } from '@renderer/utils/time-format'
import { isRoomClosed } from '@renderer/utils/room-status'

/**
 * 直播 / 回放链路的纯函数回归。
 *
 * 这些是播放器行为的「开关」：选错流会黑屏、URL 不换会被 mpegts 复用旧连接、
 * 下播判定错了会一直重试。之前全靠手工点播放验证，这里固定住口径。
 */
describe('pickPreferredStream（公演直播）', () => {
  const stream = (streamType: number, streamPath = 'rtmp://a/1') => ({ streamType, streamPath })

  it('优先高清（streamType=2）', () => {
    expect(pickPreferredStream([stream(1, 'rtmp://sd'), stream(2, 'rtmp://hd')])?.streamPath)
      .toBe('rtmp://hd')
  })

  it('没有高清时回落到第一条有地址的流', () => {
    expect(pickPreferredStream([stream(1, 'rtmp://sd')])?.streamPath).toBe('rtmp://sd')
  })

  it('高清没有地址时不算数', () => {
    expect(pickPreferredStream([stream(2, ''), stream(1, 'rtmp://sd')])?.streamPath)
      .toBe('rtmp://sd')
  })

  it('全空 / undefined 返回 null', () => {
    expect(pickPreferredStream([])).toBeNull()
    expect(pickPreferredStream(undefined)).toBeNull()
  })
})

describe('pickPreferredVodStream（公演回放）', () => {
  const vod = (streamType: number, streamPath = 'https://v/1.m3u8') => ({ streamType, streamPath })

  it('优先超清（3）', () => {
    expect(pickPreferredVodStream([vod(2), vod(3, 'https://v/4k.m3u8')])?.streamPath)
      .toBe('https://v/4k.m3u8')
  })

  it('没有超清回落高清', () => {
    expect(pickPreferredVodStream([vod(1), vod(2)])?.streamType).toBe(2)
  })

  it('全空返回 null', () => {
    expect(pickPreferredVodStream([])).toBeNull()
  })
})

describe('buildPlaybackUrl', () => {
  it('带时间戳与重启序号（避免复用旧连接）', () => {
    expect(buildPlaybackUrl('http://127.0.0.1:8080/live/abc.flv', 3, 1_700_000_000_000))
      .toBe('http://127.0.0.1:8080/live/abc.flv?t=1700000000000&r=3')
  })

  it('重启序号变化即 URL 变化', () => {
    const a = buildPlaybackUrl('http://h/x.flv', 0, 1)
    const b = buildPlaybackUrl('http://h/x.flv', 1, 1)
    expect(a).not.toBe(b)
  })
})

describe('下播 / 下架判定', () => {
  it('两类终结文案各自命中', () => {
    expect(isLiveEndedMessage('回放生成中')).toBe(true)
    expect(isLiveRemovedMessage('该成员直播已被删除')).toBe(true)
    expect(isLiveEndedMessage('该成员直播已被删除')).toBe(false)
  })

  it('user 源：回放态或已是 http 地址即视为结束', () => {
    expect(isEndedLiveDetail({ review: true, playStreamPath: 'rtmp://a/1' }, 'user')).toBe(true)
    expect(isEndedLiveDetail({ review: false, playStreamPath: 'https://v/1.m3u8' }, 'user'))
      .toBe(true)
    expect(isEndedLiveDetail({ review: false, playStreamPath: 'rtmp://a/1' }, 'user')).toBe(false)
  })

  it('open 源没有 review 概念，不算结束（交给重试链）', () => {
    expect(isEndedLiveDetail({ review: true, playStreamPath: 'https://v/1.m3u8' }, 'open'))
      .toBe(false)
  })
})

describe('isRoomClosed', () => {
  it('无留言板 = 已关闭', () => {
    expect(isRoomClosed(false)).toBe(true)
  })

  it('有留言板 = 未关闭', () => {
    expect(isRoomClosed(true)).toBe(false)
  })

  it('字段缺失（null）不据此推断，而是看服务端文案', () => {
    expect(isRoomClosed(null)).toBe(false)
    expect(isRoomClosed(null, '该房间已关闭')).toBe(true)
  })

  it('其它错误不误判成关闭', () => {
    expect(isRoomClosed(true, '网络异常')).toBe(false)
  })
})

describe('formatMediaTime', () => {
  it('不足一小时是 mm:ss，超过是 hh:mm:ss', () => {
    expect(formatMediaTime(0)).toBe('00:00')
    expect(formatMediaTime(59)).toBe('00:59')
    expect(formatMediaTime(65)).toBe('01:05')
    expect(formatMediaTime(3600)).toBe('01:00:00')
    expect(formatMediaTime(3725)).toBe('01:02:05')
  })

  it('负数与非有限值都按 0，不出现 Invalid Date', () => {
    expect(formatMediaTime(-5)).toBe('00:00')
    expect(formatMediaTime(Number.NaN)).toBe('00:00')
    expect(formatMediaTime(Number.POSITIVE_INFINITY)).toBe('00:00')
  })
})
